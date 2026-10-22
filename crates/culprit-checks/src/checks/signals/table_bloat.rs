//! The `table-bloat` check: Tables whose pages hold far fewer rows than they could, estimated from row counts and column widths in pg_stats (no extension needed). Every scan still reads the empty space.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn table_bloat_info() -> CheckInfo {
    CheckInfo {
        id: "table-bloat".into(),
        title: "Tables that are mostly empty space".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Tables whose pages hold far fewer rows than they could, estimated from row counts and column widths in pg_stats (no extension needed). Every scan still reads the empty space.".into(),
    }
}

check!(TableBloatCheck: table_bloat_info, table_bloat, &[Evidence::Tables(crate::evidence::TableMeasure::Bloat)]);

pub(super) fn table_heap_bytes(t: &TableStat) -> Option<f64> {
    t.data_bytes.or(t.total_bytes).filter(|b| *b > 0.0)
}

/// Dead rows explain the estimated empty space when their share of the row versions is at least this part of its
/// share: the estimate is good to tens of percent.
pub(crate) const DEAD_EXPLAINS_BLOAT: f64 = 0.9;

pub fn table_bloat(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let bloat = t.storage.as_ref()?.bloat_bytes?;
            let size = table_heap_bytes(t)?;
            let share = (bloat / size).min(1.0);
            ((bloat >= 100.0 * MB && share >= 0.4) || (bloat >= GB && share >= 0.3)).then_some((t, bloat, size, share))
        })
        .map(|(t, bloat, size, share)| {
            let name = t.qualified();
            let mut f = finding("table-bloat", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            f.title = format!("{} is about {} empty space (≈ {})", t.name, format::percent(share), format::bytes(bloat));
            f.facts.bloat = Some(culprit_core::FindingBloat {bytes:bloat,share});
            // The estimate counts every byte beyond the live rows, dead rows VACUUM hasn't cleared yet included. When
            // they account for it, VACUUM is the fix, not a rewrite.
            let dead = dead_share(t).filter(|d| *d >= share * DEAD_EXPLAINS_BLOAT);
            f.summary = format!(
                "From its row count and average row width, {}'s rows need about {}; it takes {}. New rows reuse freed space, but every scan still reads the empty pages, and the cache holds them. This is an estimate from statistics, good to tens of percent; pgstattuple measures it exactly.{}",
                t.name,
                format::bytes((size - bloat).max(0.0)),
                format::bytes(size),
                dead.map(|d| format!(
                    " {} of its row versions are dead rows VACUUM hasn't cleared yet, enough to be most of this space: vacuum it first and estimate again before rewriting it.",
                    format::percent(d)
                ))
                .unwrap_or_default()
            );
            f.severity = if bloat >= GB && dead.is_none() { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("≈ {} empty", format::bytes(bloat));
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new("Empty space (estimated)", bloat, Unit::Bytes),
                Metric::new("Table size", size, Unit::Bytes),
                Metric::new("Share", share, Unit::Ratio),
            ];
            let target = qualified(Engine::Postgres, &t.schema, &t.name);
            f.fix = Some(if dead.is_some() {
                Fix {
                    summary: "Vacuum it: VACUUM clears the dead rows without blocking reads or writes, and new rows then reuse their space. Make autovacuum visit it sooner so they don't pile up again. Rewrite the table (pg_repack online, VACUUM FULL offline) only if the estimate stays high after a vacuum.".into(),
                    sql: Some(format!("VACUUM (ANALYZE) {target};")),
                    cost: Some("VACUUM doesn't block reads or writes".into()),
                }
            } else {
                Fix {
                    summary: "Measure it exactly first. To give the space back, rewrite the table: pg_repack does it online; VACUUM FULL does it while blocking all reads and writes. If the table keeps growing, make autovacuum visit it more often so freed space is reused sooner.".into(),
                    sql: Some(format!(
                        "-- Exact numbers (needs the pgstattuple extension)\nSELECT * FROM pgstattuple('{}');\n-- Rewrite offline: blocks reads and writes until it finishes\n-- VACUUM (FULL, ANALYZE) {target};",
                        target.replace('\'', "''")
                    )),
                    cost: Some("Rewrites the table".into()),
                }
            });
            f.verify = vec!["Re-scan after the rewrite: the estimate should fall under 20%.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings.extend(default_partitions(snapshot));
    findings.extend(expiring_range_partitions(snapshot));
    findings.extend(brin_opportunities(snapshot));
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings
}

pub(crate) fn expiring_range_partitions(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(now_epoch) = parse_epoch_seconds(&snapshot.taken_at) else {
        return Vec::new();
    };
    let mut by_parent: std::collections::BTreeMap<String, Vec<(String, String)>> = std::collections::BTreeMap::new();
    for t in &snapshot.tables {
        if let Some(parent) = t.partition_of.as_deref() {
            if let Some(bound) = t.storage.as_ref().and_then(|s| s.partition_bounds().into_iter().next()) {
                let parent_qual = if parent.contains('.') || t.schema.is_empty() {
                    parent.to_string()
                } else {
                    format!("{}.{parent}", t.schema)
                };
                let entry = by_parent.entry(parent_qual).or_default();
                if !entry.iter().any(|(name, _)| name == &t.name) {
                    entry.push((t.name.clone(), bound.to_string()));
                }
            }
        } else if let Some(storage) = t.storage.as_ref() {
            let parent = t.qualified();
            for bound in storage.partition_bounds() {
                let (expr, part_name) = bound
                    .rsplit_once("):")
                    .map(|(left, right)| (format!("{left})"), right.to_string()))
                    .unwrap_or_else(|| (bound.to_string(), t.name.clone()));
                let entry = by_parent.entry(parent.clone()).or_default();
                if !entry.iter().any(|(name, _)| name == &part_name) {
                    entry.push((part_name, expr));
                }
            }
        }
    }
    by_parent
        .into_iter()
        .filter_map(|(parent, parts)| {
            let mut parsed = Vec::with_capacity(parts.len());
            for (part_name, bound) in &parts {
                if bound.eq_ignore_ascii_case("default")
                    || bound.to_ascii_uppercase().contains("MAXVALUE")
                {
                    return None;
                }
                let (literal, epoch) = parse_range_upper_bound(bound)?;
                parsed.push((part_name.as_str(), literal, epoch));
            }
            let (last_part, bound_literal, max_epoch) = parsed
                .into_iter()
                .max_by(|a, b| a.2.total_cmp(&b.2))?;
            let days_remaining = (max_epoch - now_epoch) / 86_400.0;
            if !(-7.0..=7.0).contains(&days_remaining) {
                return None;
            }
            let parent_short = parent.rsplit('.').next().unwrap_or(&parent);
            let bound_date = bound_literal.get(..10).unwrap_or(&bound_literal);
            let mut f = finding(
                "table-bloat",
                &format!("expiring-partition:{parent}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &parent),
            );
            if days_remaining > 0.0 {
                f.title = format!(
                    "{parent_short}'s last partition ({last_part}) ends on {bound_date} (in {})",
                    format::days(days_remaining)
                );
                f.impact.label = format!("ends in {}", format::days(days_remaining));
            } else {
                f.title = format!(
                    "{parent_short}'s last partition ({last_part}) ended on {bound_date}"
                );
                f.impact.label = format!("ended {bound_date}");
            }
            f.severity = if days_remaining <= 2.0 { Severity::High } else { Severity::Medium };
            severity_score(&mut f);
            f.confidence = Confidence::High;
            f.summary = format!(
                "{parent} has {} range {} and no DEFAULT or MAXVALUE partition. Its latest partition ({last_part}) \
                 covers values up to '{bound_literal}': once timestamps reach '{bound_literal}', INSERTs into \
                 {parent} will fail with a partition-routing error.",
                parts.len(),
                if parts.len() == 1 { "partition" } else { "partitions" },
            );
            f.metrics = vec![
                Metric::new("Partitions", parts.len() as f64, Unit::Count),
                Metric::new("Days until upper bound", days_remaining.max(0.0), Unit::Days),
            ];
            f.fix = Some(Fix {
                summary: format!(
                    "Create the next range partition(s) on {parent} covering values from '{bound_literal}' onward, \
                     and check the scheduled job or partition manager that pre-creates future partitions."
                ),
                sql: None,
                cost: Some("Creating a new empty partition is fast metadata DDL.".into()),
            });
            f.verify = vec![format!(
                "Re-scan after creating future partitions: {parent}'s highest partition bound should be more than 7 days ahead."
            )];
            Some(f)
        })
        .collect()
}

fn parse_range_upper_bound(bound: &str) -> Option<(String, f64)> {
    let upper = bound.to_ascii_uppercase();
    let after = if upper.starts_with("FOR VALUES FROM") {
        let to_pos = upper.find(" TO (")?;
        bound.get(to_pos + 5..)?.trim_start()
    } else {
        let less_pos = upper.find("VALUES LESS THAN")?;
        let rest = bound.get(less_pos + "VALUES LESS THAN".len()..)?.trim_start();
        rest.strip_prefix('(')?.trim_start()
    };
    let literal = after.strip_prefix('\'')?.split('\'').next()?.trim();
    if literal.is_empty() {
        return None;
    }
    let epoch = if literal.contains(['T', ' ']) {
        parse_epoch_seconds(literal)?
    } else if literal.len() == 10 {
        parse_epoch_seconds(&format!("{literal}T00:00:00Z"))?
    } else {
        return None;
    };
    Some((literal.to_string(), epoch))
}

fn default_partitions(snapshot: &Snapshot) -> Vec<Finding> {
    snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let parent = t.partition_of.as_deref()?;
            let is_default = t.storage.as_ref().is_some_and(|s| s.is_default_partition());
            if !is_default {
                return None;
            }
            let rows = t.rows.or_else(|| t.storage.as_ref().and_then(|s| s.planner_rows)).unwrap_or(0.0);
            let size = table_heap_bytes(t).unwrap_or(0.0);
            if rows < 100_000.0 && size < 100.0 * MB {
                return None;
            }
            let name = t.qualified();
            let mut f = finding(
                "table-bloat",
                &format!("default-partition:{name}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &name),
            );
            let size_note = if size > 0.0 { format!(" ({})", format::bytes(size)) } else { String::new() };
            f.title = format!("DEFAULT partition {} holds {} rows{size_note}", t.name, format::count(rows));
            f.severity = if rows >= 1_000_000.0 || size >= GB { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = if size > 0.0 {
                format!("{} in DEFAULT", format::bytes(size))
            } else {
                format!("{} rows in DEFAULT", format::count(rows))
            };
            f.confidence = Confidence::High;
            f.summary = format!(
                "{} is the DEFAULT partition of {parent}, and holds {} rows{size_note}. Rows whose partition key has \
                 no matching partition land here — often when scheduled partition creation falls behind. In PostgreSQL, \
                 adding or attaching a new partition on {parent} scans the DEFAULT partition under an ACCESS EXCLUSIVE \
                 lock and fails if any rows in {} belong in the new partition's range.",
                t.name,
                format::count(rows),
                t.name
            );
            if rows > 0.0 {
                f.metrics.push(Metric::new("Rows in DEFAULT partition", rows, Unit::Rows));
            }
            if size > 0.0 {
                f.metrics.push(Metric::new("Size", size, Unit::Bytes));
            }
            f.fix = Some(Fix {
                summary: format!(
                    "Create the missing partitions on {parent}, move the matching rows out of {} into them (or detach \
                     {}, split its rows, and re-attach), and check the job that creates future partitions ahead of time.",
                    t.name, t.name
                ),
                sql: None,
                cost: Some("Moving rows or attaching partitions locks the affected partitions.".into()),
            });
            f.verify = vec![format!("Re-scan after moving the rows out: {} should be empty or small.", t.name)];
            Some(f)
        })
        .collect()
}

fn is_time_series_column(snapshot: &Snapshot, schema: &str, table: &str, col_name: &str) -> bool {
    if col_name.is_empty() || !col_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    if snapshot.columns.iter().any(|c| {
        c.schema == schema
            && c.table == table
            && c.name.eq_ignore_ascii_case(col_name)
            && (c.data_type.to_ascii_lowercase().contains("timestamp") || c.data_type.eq_ignore_ascii_case("date"))
    }) {
        return true;
    }
    matches!(
        col_name.to_ascii_lowercase().as_str(),
        "created_at" | "inserted_at" | "occurred_at" | "logged_at" | "recorded_at" | "event_time" | "timestamp"
    )
}

fn brin_opportunities(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Postgres {
        return Vec::new();
    }
    let mut out = Vec::new();
    for t in &snapshot.tables {
        let rows = t.rows.or_else(|| t.storage.as_ref().and_then(|s| s.planner_rows)).unwrap_or(0.0);
        let size = table_heap_bytes(t).unwrap_or(0.0);
        if !rows.is_finite() || !size.is_finite() || rows < 5_000_000.0 || size < 500.0 * MB {
            continue;
        }
        let table_qual = t.qualified();
        // Skip partitioned parent tables (where CREATE INDEX CONCURRENTLY is invalid in PostgreSQL).
        if snapshot.tables.iter().any(|child| child.partition_of.as_deref() == Some(table_qual.as_str()))
            || t.storage.as_ref().is_some_and(|s| !s.partition_bounds().is_empty() && t.partition_of.is_none())
        {
            continue;
        }
        let inserts = t.inserts.unwrap_or(0.0);
        let updates = t.updates.unwrap_or(0.0);
        let deletes = t.deletes.unwrap_or(0.0);
        if !inserts.is_finite()
            || !updates.is_finite()
            || !deletes.is_finite()
            || inserts < 0.0
            || updates < 0.0
            || deletes < 0.0
        {
            continue;
        }
        let total_writes = inserts + updates + deletes;
        if !total_writes.is_finite() || total_writes < 100_000.0 || (updates + deletes) / total_writes > 0.02 {
            continue;
        }
        if t.dead_rows.unwrap_or(0.0) / rows > 0.02 {
            continue;
        }
        for idx in &snapshot.indexes {
            if idx.schema != t.schema || idx.table != t.name {
                continue;
            }
            // Skip unique/PK/disabled/partial/covering/custom-opclass/partitioned or unused indexes (which unused-index handles).
            if idx.is_unique
                || idx.is_primary_key
                || idx.is_disabled
                || idx.filter.is_some()
                || !idx.include.is_empty()
                || idx.opclasses.is_some()
                || idx.partition_of.is_some()
                || idx.reads == Some(0.0)
                || idx.columns.len() != 1
                || (!idx.kind.is_empty() && !idx.kind.eq_ignore_ascii_case("btree"))
            {
                continue;
            }
            let Some(idx_bytes) = idx.size_bytes.filter(|b| b.is_finite() && *b >= 100.0 * MB) else {
                continue;
            };
            let col_name = culprit_core::strip_direction(&idx.columns[0]);
            if !is_time_series_column(snapshot, &t.schema, &t.name, col_name) {
                continue;
            }
            let has_brin = snapshot.indexes.iter().any(|other| {
                other.schema == t.schema
                    && other.table == t.name
                    && other.kind.eq_ignore_ascii_case("brin")
                    && other.key_names().first().is_some_and(|k| k.eq_ignore_ascii_case(col_name))
            });
            if has_brin {
                continue;
            }
            let mut f = finding(
                "table-bloat",
                &format!("brin-candidate:{table_qual}.{}", idx.name),
                Category::Maintenance,
                subject(SubjectKind::Index, format!("{table_qual}.{}", idx.name)),
            );
            f.title = format!(
                "{} on append-only {} takes {} as a B-tree (a BRIN index would take < 1%)",
                idx.name,
                t.name,
                format::bytes(idx_bytes)
            );
            f.severity = if idx_bytes >= GB { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("{} B-tree on append-only time column", format::bytes(idx_bytes));
            f.confidence = Confidence::Medium;
            f.summary = format!(
                "{} holds {} rows ({}) and its recorded writes are {} inserts ({} inserts vs {} updates/deletes). \
                 {} is a {} B-tree index on `{col_name}`: because rows in an append-only table are stored on heap pages \
                 in roughly `{col_name}` order, a Block Range Index (BRIN) on `{col_name}` stores only the minimum and \
                 maximum timestamp per page range — taking kilobytes instead of {} while still skipping non-matching \
                 heap blocks on time-range queries.",
                t.name,
                format::count(rows),
                format::bytes(size),
                format::percent(inserts / total_writes),
                format::count(inserts),
                format::count(updates + deletes),
                idx.name,
                format::bytes(idx_bytes),
                format::bytes(idx_bytes),
            );
            f.metrics = vec![
                Metric::new("B-tree index size", idx_bytes, Unit::Bytes),
                Metric::new("Table rows", rows, Unit::Rows),
                Metric::new("Inserts", inserts, Unit::Rows),
            ];
            let target = qualified(Engine::Postgres, &t.schema, &t.name);
            let brin_name = quote(Engine::Postgres, &format!("ix_{}_{}_brin", t.name, col_name));
            let quoted_col = quote(Engine::Postgres, col_name);
            f.fix = Some(Fix {
                summary: format!(
                    "If `{col_name}` is queried by time ranges rather than point lookups and rows are inserted in \
                     timestamp order, build a BRIN index concurrently and verify with `EXPLAIN` that range queries use \
                     a Bitmap Index Scan on it before retiring `{}`.",
                    idx.name
                ),
                sql: Some(format!("CREATE INDEX CONCURRENTLY {brin_name} ON {target} USING brin ({quoted_col});")),
                cost: Some(
                    "CONCURRENTLY scans the table without blocking reads or writes; the resulting BRIN index is tiny."
                        .into(),
                ),
            });
            f.verify = vec![format!(
                "Re-scan after building `{}`: verify time-range queries use it before dropping `{}`.",
                brin_name.trim_matches('"'),
                idx.name
            )];
            out.push(f);
        }
    }
    out
}
