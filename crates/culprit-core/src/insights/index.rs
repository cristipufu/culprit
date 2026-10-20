//! Plan insights about indexes: lookups back to the table, scans that filter, the optimizer's index requests and
//! the indexes an engine builds for itself.

use super::*;

/// A `CREATE INDEX` to copy, in the engine's dialect. A plan doesn't say the server's edition, so SQL Server's online
/// build is suggested, not assumed. Culprit never runs it.
pub(super) fn index_sql(engine: Engine, table: &str, keys: &[String], include: &[String]) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    // Named as the findings name the same index (`support::index_name` in culprit-checks), so a plan's insight and the
    // finding about it never spell one index two ways: `IX_`, the table and the keys, lower case where the engine folds
    // names.
    let name: String = std::iter::once(format!("IX_{}", table_leaf(table)))
        .chain(keys.iter().map(|k| k.replace(' ', "")))
        .collect::<Vec<_>>()
        .join("_")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .take(60)
        .collect();
    let name = if engine == Engine::Mssql { name } else { name.to_ascii_lowercase() };
    let target = qualified(engine, table);
    Some(engine.dialect().create_index(&crate::dialect::NewIndex {
        target: &target,
        unique: false,
        name: &name,
        keys,
        include,
        filter: None,
        online: crate::dialect::Online::Unknown,
    }))
}

/// Output columns of an operator (`OutputList` on SQL Server), as plain names.
pub(super) fn output_columns(n: &PlanNode) -> Vec<String> {
    n.facts()
        .output_columns
        .as_ref()
        .map(|list| list.iter().filter(|c| !c.is_empty() && !c.starts_with("Expr")).cloned().collect())
        .unwrap_or_default()
}

/// Rows a lookup step fetches one at a time: its executions, or the rows it returns when that's more.
fn lookups(n: &PlanNode) -> f64 {
    n.act_executions.unwrap_or(n.est_executions).max(rows_out(n))
}

/// Whether a key or RID lookup fetches enough rows one at a time to be worth covering: the `key-lookups` rule's bar.
fn many_lookups(n: &PlanNode) -> bool {
    n.kind == NodeKind::Lookup && n.op.contains("Lookup") && lookups(n) >= MANY_RUNS
}

pub(super) fn key_lookups(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| many_lookups(n)) {
        let runs = lookups(n);
        let table = what(n);
        let cols = output_columns(n);
        let seek = cx.parent(n).and_then(|p| p.children.first()).filter(|s| s.id != n.id && s.index.is_some());
        let index = seek.and_then(|s| s.index.clone());
        let impact = cx.share(n) + cx.parent(n).filter(|p| p.kind == NodeKind::Join).map_or(0.0, |p| cx.share(p));
        let mut nodes = vec![n.id];
        nodes.extend(seek.map(|s| s.id));
        // The share only where the plan states one (`stated_share`).
        let together = if cx.states_share(&nodes) {
            format!(" Together that's {} of this plan's work.", format::percent(impact))
        } else {
            String::new()
        };
        let mut d = Draft::new("key-lookups", InsightArea::Index, impact, nodes);
        // An estimated plan says how many lookups the optimizer expects, not how many a run made.
        let measured = n.act_executions.is_some() || n.act_rows.is_some();
        d.title = format!(
            "{} {} in {table} one at a time",
            if measured { "Looks up" } else { "Estimated to look up" },
            plural(runs, "row", "rows")
        );
        let heap = n.op.starts_with("RID");
        d.detail = match (&index, cols.is_empty()) {
            (Some(ix), false) => format!(
                "{ix} finds the rows but doesn't hold {}, so SQL Server goes back to {table} for each one ({}).{together}",
                format::list(&cols),
                n.op,
            ),
            _ => format!("Each row found by the index needs a second read from {table} ({}).{together}", n.op),
        };
        // Said in words only: rebuilding the index needs its whole definition (key order, existing included columns,
        // uniqueness, filter, compression), which the plan doesn't carry.
        let include = if cols.is_empty() { "the looked-up columns".to_string() } else { format::list(&cols) };
        d.fix = match (&index, heap) {
            (_, true) => format!(
                "{table} has no clustered index, so every lookup follows a row pointer. Give it a clustered index, or add {include} to {} as included columns.",
                index.as_deref().unwrap_or("the index")
            ),
            (Some(ix), false) => format!(
                "Add {include} to {ix} as included columns (keep its keys and its existing included columns) so the seek answers the query by itself, or select fewer columns."
            ),
            (None, false) => format!("Add {include} to the index as included columns, or select fewer columns."),
        };
        d.verify = format!(
            "Explain again: the {} is gone and {} returns the rows by itself.",
            n.op,
            index.as_deref().unwrap_or("the seek")
        );
        out.push(d);
    }
    out
}

pub(super) fn missing_index_hint(cx: &Cx) -> Vec<Draft> {
    // A key lookup on the same table with the same keys already proposes this index, with the columns it needs.
    let lookups: Vec<(String, Vec<String>)> = cx
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Lookup)
        .filter_map(|n| {
            let seek = cx.parent(n)?.children.first().filter(|s| s.id != n.id)?;
            let keys = filter_columns(&conditions(seek)).into_iter().map(|k| k.to_ascii_lowercase()).collect();
            Some((table_leaf(&what(n)).to_ascii_lowercase(), keys))
        })
        .collect();
    let mut hints: Vec<_> = cx
        .plan
        .index_hints
        .iter()
        .filter(|h| {
            let keys: Vec<String> =
                h.equality.iter().chain(&h.inequality).map(|c| clean_identifier(c).to_ascii_lowercase()).collect();
            !lookups.iter().any(|(t, k)| *t == table_leaf(&h.table).to_ascii_lowercase() && *k == keys)
        })
        .collect();
    hints.sort_by(|a, b| b.impact_pct.total_cmp(&a.impact_pct));
    hints
        .into_iter()
        .take(3)
        .map(|h| {
            let leaf = table_leaf(&h.table).to_ascii_lowercase();
            let node = cx
                .nodes
                .iter()
                .filter(|n| n.object.as_deref().is_some_and(|o| table_leaf(o).to_ascii_lowercase() == leaf))
                .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
                .copied();
            let keys: Vec<String> = h.equality.iter().chain(&h.inequality).map(|c| clean_identifier(c)).collect();
            let include: Vec<String> = h.include.iter().map(|c| clean_identifier(c)).collect();
            let impact = node.map_or(0.0, |n| cx.share(n)).max(h.impact_pct / 100.0 * 0.5);
            let mut d = Draft::new("missing-index-hint", InsightArea::Index, impact, node.map(|n| vec![n.id]).unwrap_or_default());
            d.index_hint = Some(h.clone());
            d.title = format!(
                "SQL Server estimates an index on {} ({}) would make this {} cheaper",
                h.table,
                format::list(&keys),
                format::percent(h.impact_pct / 100.0)
            );
            d.detail = format!(
                "The optimizer noticed it had to read {} without a fitting index{}. Its estimate is a guide, not a promise: check the columns against the other queries on {}.",
                h.table,
                node.map(|n| format!(" ({} rows read by {})", format::count(rows_read(n)), n.op)).unwrap_or_default(),
                table_leaf(&h.table)
            );
            d.fix = if include.is_empty() {
                format!("Create the index on {}.", format::list(&keys))
            } else {
                format!("Create the index on {} and include {}.", format::list(&keys), format::list(&include))
            };
            d.fix_sql = index_sql(Engine::Mssql, &h.table, &keys, &include);
            d.verify = format!("Explain again: the suggestion is gone and the read of {} becomes a seek.", table_leaf(&h.table));
            d
        })
        .collect()
}

/// The filter a scan applies: its own, or MySQL EXPLAIN ANALYZE's Filter node above it.
pub(super) fn scan_filter_text(cx: &Cx, n: &PlanNode) -> Option<String> {
    n.predicate.clone().or_else(|| cx.parent(n).filter(|p| p.op == "Filter").and_then(|f| f.predicate.clone()))
}

pub(super) fn scan_filter(cx: &Cx) -> Vec<Draft> {
    let hinted: Vec<String> = cx.plan.index_hints.iter().map(|h| table_leaf(&h.table).to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| index_could_pay(cx, n)) {
        let Some(table) = n.object.clone() else { continue };
        let Some(filter) = scan_filter_text(cx, n) else { continue };
        // SQL Server's own suggestion, a conversion, a function or a leading wildcard explain the scan better, and
        // MySQL's ignored index says why there's no seek: their rules say so.
        if hinted.contains(&table_leaf(&table).to_ascii_lowercase())
            || blocker(cx, n, &filter).is_some()
            || (cx.engine() == Engine::Mysql && !possible_keys_for_filter(cx, n).is_empty())
        {
            continue;
        }
        // Only the conditions an index can seek on make the advice; a filter with none of them has no index to give.
        let cols = seekable_columns(&filter);
        if cols.is_empty() {
            continue;
        }
        let read = rows_read(n);
        let kept = read * filter_keeps(cx, n).unwrap_or(1.0);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("scan-filter", InsightArea::Index, impact, vec![n.id]);
        d.title = format!("Reads {} of {table} to keep {}", plural(read, "row", "rows"), rows_text(kept));
        let estimated = n.act_rows.is_none();
        // What the statement's runs measured of the filter, where the analysis put it on the scan.
        let measured = estimated && n.facts().measured_kept_share.is_some();
        d.detail = format!(
            "{} {} reads the rows and checks `{}` on each one, so most of the reading is thrown away.{}",
            if measured {
                "As the statement's runs measured, the"
            } else if estimated {
                "As planned, the"
            } else {
                "The"
            },
            n.op,
            format::predicate(&filter),
            cx.work_sentence(impact, &[n.id])
        );
        // What the catalog says the table has, where the snapshot marked it: an index that already starts with one of
        // these columns, or none.
        let leaf = table_leaf(&table);
        let has = table_indexes(n).map(|list| {
            list.into_iter()
                .find(|(_, keys)| keys.first().is_some_and(|k| cols.iter().any(|c| c.eq_ignore_ascii_case(k))))
        });
        // Equality columns go in any order: the filter's isn't the data's. The SQL below takes the filter's order, and
        // the note says so rather than advise an order the SQL doesn't have.
        let (equality, _) = seekable_keys(&filter);
        let order = if equality.len() >= 2 {
            format!(
                " {} can go in any order, and the SQL below takes the filter's ({} first). Put {} first instead if it \
                 has more distinct values, or if another index you add on {leaf} leads with it: then that one index \
                 serves this query too.",
                format::list(&equality),
                equality[0],
                if equality.len() == 2 { equality[1].as_str() } else { "another one" }
            )
        } else {
            String::new()
        };
        match has {
            Some(Some((index, keys))) => {
                d.fix = format!(
                    "{index} already starts with {}, yet this plan reads {leaf} instead: refresh {leaf}'s statistics \
                     first, then check that the filter compares the column as it is (no function or conversion on it).",
                    keys[0]
                );
                d.fix_sql = Some(cx.engine().dialect().refresh_statistics(&qualified(cx.engine(), &table)));
            }
            Some(None) => {
                d.fix = format!(
                    "No index on {leaf} starts with {}: one on {} lets the database go straight to the matching rows.{order}",
                    format::list(&cols).replace(" and ", " or "),
                    format::list(&cols)
                );
                d.fix_sql = index_sql(cx.engine(), &table, &cols, &[]);
            }
            None => {
                d.fix = format!(
                    "An index on {} lets the database go straight to the matching rows; if one already exists, the optimizer didn't use it, so refresh the table's statistics first.{order}",
                    format::list(&cols)
                );
                d.fix_sql = index_sql(cx.engine(), &table, &cols, &[]);
            }
        }
        d.verify = format!("Explain again: {} reads about as many rows as it keeps.", table_leaf(&table));
        out.push(d);
    }
    out
}

pub(super) fn big_sort(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    // An actual plan with operator timings says what the sort took; its cost share was estimated for the rows the
    // optimizer expected, which can be many times the rows it sorted.
    let measured = crate::actual::self_times(cx.plan);
    let total_ms: f64 = measured.as_ref().map_or(0.0, |t| t.values().sum());
    for n in cx.nodes.iter().filter(|n| n.kind == NodeKind::Sort) {
        if n.warnings.contains(&PlanWarning::Spill)
            || n.detail.as_deref().is_some_and(|d| d.contains("temporary table"))
        {
            continue; // spill and temp-table-sort say more
        }
        // Rows expected from a scan planned on a default guess (a parameter compared by range in a generic plan), or a
        // sort run once per row of one, are the guess, not the data: no row count to state, nor a share of the work
        // built on it, until a run measures them.
        if n.act_rows.is_none() && cx.guessed.contains(&n.id) {
            continue;
        }
        let rows = n.children.first().map_or(rows_out(n), rows_out).max(rows_out(n));
        let share = match &measured {
            Some(times) if total_ms > 0.0 => times.get(&n.id).map_or(0.0, |ms| (ms / total_ms).min(1.0)),
            _ => cx.share(n),
        };
        if share < 0.2 || rows < 50_000.0 {
            continue;
        }
        let mut d = Draft::new("big-sort", InsightArea::Index, share, vec![n.id]);
        d.title = if cx.states_share(&[n.id]) {
            format!("Sorting {} rows is {} of this plan's work", format::count(rows), format::percent(share))
        } else {
            format!("Sorts {} rows", format::count(rows))
        };
        let key =
            n.detail.as_deref().and_then(|d| d.split(" · ").find(|p| p.starts_with("sort key")).map(str::to_string));
        d.detail = format!(
            "The rows arrive in a different order than the query needs{}, so they're all collected and sorted before the first one comes out.",
            key.map(|k| format!(" ({k})")).unwrap_or_default()
        );
        d.fix = "An index whose key order matches the ORDER BY (with the WHERE equality columns first) returns rows already sorted; or sort fewer rows by filtering earlier or limiting the result.".into();
        d.verify = "Explain again: no Sort, or one over far fewer rows.".into();
        out.push(d);
    }
    out
}

pub(super) fn eager_spool(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.op == "Index Spool" && n.detail.as_deref() == Some("Eager Spool")) {
        let table = first_object(n).unwrap_or_else(|| "the table".into());
        // The spool's keys as columns of the table: an optimizer value (Expr1003, computed below the spool) is no
        // column an index on the table could have.
        let keys: Vec<String> = filter_columns(n.seek_predicate.as_deref().unwrap_or(""))
            .into_iter()
            .filter(|k| is_column_operand(k))
            .collect();
        let include = output_columns(n);
        let rows = n.children.first().map_or(0.0, rows_out);
        let impact = cx.subtree(n);
        let mut d = Draft::new("eager-spool", InsightArea::Index, impact, vec![n.id]);
        d.floor = Severity::Medium;
        d.title = format!("SQL Server builds a temporary index on {} every time this runs", table_leaf(&table));
        d.detail = format!(
            "The Index Spool copies {} rows of {} into a temporary index in tempdb on each run and drops it afterwards, because no index fits the lookup{}.",
            format::count(rows),
            table_leaf(&table),
            if keys.is_empty() { String::new() } else { format!(" on {}", format::list(&keys)) }
        );
        d.fix = if keys.is_empty() {
            "It seeks on a value computed in the plan, not on a column of the table: rewrite the join or subquery above the spool to compare columns, so an index on them can replace the spool.".into()
        } else {
            "Create that index permanently, so the work happens once instead of on every run.".into()
        };
        d.fix_sql = index_sql(Engine::Mssql, &table, &keys, &include);
        d.verify = "Explain again: the Index Spool is gone and the lookup seeks the new index.".into();
        out.push(d);
    }
    out
}

/// SQLite built a throwaway index for this statement (`AUTOMATIC COVERING INDEX ON t(x)`): by its own account no index
/// fits, so it builds one on each run and drops it. Its plans carry no row counts, so how much that costs is unknown
/// here: no floor, unlike SQL Server's eager spool. A scan weighs it by the statement's share of database time.
pub(super) fn automatic_index(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for EngineBuiltIndex { node, table, keys } in engine_built_indexes(cx.plan) {
        let mut d = Draft::new("automatic-index", InsightArea::Index, 1.0, vec![node]);
        d.fixed_weight = true;
        d.title = format!("SQLite builds a temporary index on {} every time this runs", table_leaf(&table));
        d.detail = format!(
            "No index on {} fits this {}, so SQLite builds one{} for each run and throws it away.",
            table_leaf(&table),
            if cx.statement().contains("join") { "join" } else { "search" },
            if keys.is_empty() { String::new() } else { format!(" on {}", format::list(&keys)) }
        );
        d.fix = "Create that index once, so the work happens one time instead of on every run.".into();
        d.fix_sql = index_sql(Engine::Sqlite, &table, &keys, &[]);
        d.verify = "Explain again: no AUTOMATIC INDEX, and a SEARCH using the new index.".into();
        out.push(d);
    }
    out
}

pub(super) fn many_to_many(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op == "Merge Join" && (n.facts().many_to_many == Some(true)))
        .take(1)
        .map(|n| {
            let mut d = Draft::new("many-to-many", InsightArea::Index, cx.share(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = "The merge join is many-to-many, so it replays duplicates through a tempdb worktable".into();
            d.detail = "When neither input is unique on the join columns, SQL Server rewinds the inner side for every duplicate on the outer side.".into();
            d.fix = "If one side really is unique on the join columns, declare it (a unique index or constraint); otherwise a hash join is usually cheaper.".into();
            d.verify = "Explain again: the Merge Join no longer shows ManyToMany.".into();
            d
        })
        .collect()
}

pub(super) fn join_buffer(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx
        .nodes
        .iter()
        .filter(|n| n.facts().join_buffer.is_some() && (n.predicate.is_some() || n.seek_predicate.is_some()))
    {
        let table = what(n);
        let how = n.facts().join_buffer.clone().unwrap_or_default();
        // One read of a small table into the buffer costs too little for an index to be worth having. Batched Key
        // Access buffers the rows joined so far and looks each one up through an index: no full read to speak of.
        if rows_read(n) < MANY_ROWS || how.contains("Batched Key Access") {
            continue;
        }
        let cond = n.predicate.clone().unwrap_or_default();
        let own = table_leaf(&table).to_ascii_lowercase();
        let alias = n.facts().table_alias.as_ref().map(|a| a.to_ascii_lowercase());
        // Columns of this table in the join condition: `t`.`col` where t is this table's name or alias.
        let cols: Vec<String> = cond
            .split(" and ")
            .flat_map(|part| {
                part.split(['=', '<', '>'])
                    .map(str::trim)
                    .filter(|side| {
                        let l = side.to_ascii_lowercase();
                        l.contains(&format!("{own}`."))
                            || l.contains(&format!("{own}."))
                            || alias
                                .as_ref()
                                .is_some_and(|a| l.contains(&format!("{a}`.")) || l.contains(&format!("{a}.")))
                    })
                    .map(column_leaf)
                    .collect::<Vec<_>>()
            })
            .collect();
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("join-buffer", InsightArea::Index, impact, vec![n.id]);
        let leaf = table_leaf(&table);
        let join_cols = if cols.is_empty() { "the join column".to_string() } else { format::list(&cols) };
        // MySQL listed indexes it could use on this table, yet joins through the buffer: it judged a full read
        // cheaper, or those indexes serve another condition. Either way the plan doesn't show an index is missing.
        match (!n.facts().possible_index_names.is_empty()).then(|| n.facts().possible_index_names.join(", ")) {
            Some(keys) => {
                d.title = format!("Joins {leaf} through a join buffer ({how}) over a full read instead of an index");
                d.detail = format!(
                    "MySQL listed {keys} as possible for {leaf}, yet reads all {} rows into a join buffer and matches them there, rather than looking up each row's matches.",
                    format::count(rows_read(n))
                );
                d.fix = format!(
                    "Check whether {keys} starts with {join_cols}: if not, an index that does lets MySQL look rows up directly; if it does, refresh {leaf}'s statistics so MySQL weighs it right."
                );
                d.fix_sql =
                    n.object.as_ref().map(|t| Engine::Mysql.dialect().refresh_statistics(&qualified(Engine::Mysql, t)));
            }
            None => {
                // An index pays for a join when the rows to match are few next to the table: one lookup each against
                // one full read. With as many to match as a tenth of what it reads, the hash join is the better plan.
                let before = cx.parent(n).and_then(|p| {
                    let at = p.children.iter().position(|c| c.id == n.id)?;
                    (at > 0).then(|| rows_out(&p.children[at - 1]))
                });
                if before.is_some_and(|outer| outer * 10.0 >= rows_read(n)) {
                    continue;
                }
                d.title = format!("Joins {leaf} without an index ({how} over a full read)");
                d.detail = format!(
                    "MySQL has no index to look up matching rows of {leaf} for each row it joins, so it reads all {} rows into a join buffer and matches them there.",
                    format::count(rows_read(n))
                );
                d.fix = format!("Index {leaf} on {join_cols} so MySQL can look rows up directly.");
                d.fix_sql = index_sql(Engine::Mysql, &table, &cols, &[]);
            }
        }
        d.verify =
            format!("Explain again: {} is read with ref or eq_ref access, without a join buffer.", table_leaf(&table));
        out.push(d);
    }
    out
}
