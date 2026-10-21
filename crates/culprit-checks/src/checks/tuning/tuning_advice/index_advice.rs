//! Azure SQL automatic tuning index recommendations (`CreateIndex` and `DropIndex`), weighed against the optimizer's
//! missing-index requests, existing indexes, and measured statement runs.

use super::*;

/// Azure SQL's waiting recommendation to create an index on these key columns of a table. The missing-index finding for
/// the same index says so rather than a second finding.
pub(crate) fn recommended_create<'a>(
    snapshot: &'a Snapshot,
    schema: &str,
    table: &str,
    keys: &[String],
) -> Option<&'a TuningRecommendation> {
    snapshot.tuning.as_ref()?.recommendations.iter().find(|r| {
        waiting(r)
            && r.kind.eq_ignore_ascii_case("CreateIndex")
            && r.index.as_ref().is_some_and(|i| {
                i.schema.eq_ignore_ascii_case(schema)
                    && i.table.eq_ignore_ascii_case(table)
                    && !i.columns.is_empty()
                    && lower_set(&i.columns) == lower_set(keys)
            })
    })
}

/// Azure's score below which it expects an index to save little next to what it costs every write.
const LOW_SCORE: f64 = 10.0;

/// An average run under this is fast: no slow read for an index to speed up (the bar from which a plan does real work,
/// `culprit_core::insights`' ten milliseconds).
const SLOW_RUN_MS: f64 = 10.0;

/// Statements listed under a recommendation, the heaviest first; the rest are counted.
const LISTED: usize = 5;

pub(super) fn create_index_advice(
    snapshot: &Snapshot,
    r: &TuningRecommendation,
    ix: &RecommendedIndex,
) -> Option<Finding> {
    if ix.columns.is_empty() {
        return None;
    }
    // The optimizer asks for the same index: the missing-index finding says Azure recommends it too.
    if merge_candidates(snapshot).iter().any(|c| {
        c.schema.eq_ignore_ascii_case(&ix.schema)
            && c.table.eq_ignore_ascii_case(&ix.table)
            && lower_set(&c.keys()) == lower_set(&ix.columns)
    }) {
        return None;
    }
    let table_name = format!("{}.{}", ix.schema, ix.table);
    let columns = format::list(&ix.columns);
    let mut f = finding(
        "tuning-advice",
        &format!("create-index:{table_name}({})", ix.columns.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    f.title = format!("Azure SQL recommends an index on {} ({columns})", ix.table);
    let including =
        if ix.include.is_empty() { String::new() } else { format!(", including {}", format::list(&ix.include)) };
    // The statements behind it, where the scan captured them: those that compare every key with a value. One heavy
    // enough, and measured reading pages for nothing, is evidence of its own that the index would pay.
    let filtering = statements_filtering_on(snapshot, ix);
    let keys = if ix.columns.len() == 1 { columns.clone() } else { format!("all of {columns}") };
    let behind = match filtering.len() {
        0 => format!(
            "none of the statements this scan captured compares {keys} with a value: the ones it was recommended for \
             may have changed, stopped running, or not be among them"
        ),
        1 => format!("{} (below) compares {keys} with a value", query_handle(filtering[0])),
        n if n <= LISTED => format!("{n} statements this scan captured (below) compare {keys} with a value"),
        n => format!(
            "{n} statements this scan captured compare {keys} with a value; the {LISTED} that take the most time are \
             below"
        ),
    };
    f.summary = format!(
        "Azure SQL Database's automatic tuning recommends an index on {table_name} ({columns}{including}){}{}. It weighs \
         what the queries that read {} would save against what the index costs every write. The optimizer's own \
         missing-index requests since the last restart don't include it; {behind}.",
        r.valid_since
            .as_deref()
            .map(|s| format!(", since {}", format::friendly_date(s, &snapshot.taken_at)))
            .unwrap_or_default(),
        r.score.map(|s| format!(", and scores it {s:.0} out of 100")).unwrap_or_default(),
        ix.table
    );
    f.queries = query_refs(&filtering.iter().take(LISTED).copied().collect::<Vec<_>>());
    let rows = snapshot.table(&ix.schema, &ix.table).and_then(|t| t.rows);
    // Large values stay out of INCLUDE, as in the optimizer's requests: copying them into an index costs more than the
    // lookups back to the table it spares.
    let mut include = ix.include.clone();
    let large_note = crate::indexes::drop_large_includes(snapshot, &ix.schema, &ix.table, &mut include);
    let large: Vec<String> = ix.include.iter().filter(|c| !include.contains(c)).cloned().collect();
    // An index that leads with the keys may hold the included columns too: then the recommendation predates it (a
    // migration added it since), and it expires on its own; the clustered index holds every column. One that lacks
    // them can be widened instead.
    let same_keys = leading_index(snapshot, &ix.schema, &ix.table, &ix.columns, &ix.columns);
    let lacking: Vec<String> = match same_keys {
        Some(existing) => {
            let held: Vec<&str> =
                existing.key_names().into_iter().chain(existing.include.iter().map(String::as_str)).collect();
            include.iter().filter(|c| !held.iter().any(|h| h.eq_ignore_ascii_case(c))).cloned().collect()
        }
        None => Vec::new(),
    };
    if same_keys.is_some_and(|e| e.is_clustered) || (same_keys.is_some() && lacking.is_empty() && large.is_empty()) {
        return None;
    }
    // Where an index with these keys exists, a statement reading for nothing isn't missing this one.
    let heavy = heavy_reader(snapshot, &filtering).filter(|_| same_keys.is_none());
    // Nor is one whose plan reads the table through another index while one that leads with keys it compares with a
    // value is there: what it reads comes from the plan's choice (the heavy-query finding's to fix), so it is no
    // evidence for creating this one.
    let picked = heavy.as_ref().and_then(|h| picked_elsewhere(snapshot, ix, h));
    let heavy = heavy.filter(|_| picked.is_none());
    f.severity = if r.score.is_some_and(|s| s >= 50.0) { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + r.score.unwrap_or(0.0) / 10.0;
    f.impact.label = "Recommended by Azure SQL".into();
    f.confidence = Confidence::Medium;
    if let Some(score) = r.score {
        f.metrics.push(Metric::new("Azure's score (0–100)", score, Unit::Count));
    }
    if let Some(rows) = rows {
        f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
    }
    // Azure's score weighs what it expects the index to save against its upkeep: a low one is its own word that the
    // index is hardly worth having, unless a heavy statement the scan measured on exactly these keys says otherwise.
    let low = r.score.filter(|s| *s < LOW_SCORE);
    let rated = match (low, &heavy) {
        (Some(s), Some(h)) => format!(
            "Azure scores it {s:.0} out of 100, for what it expects the index to save next to what it costs every \
             write, but {}, query {} below, filters on exactly these columns, takes {} of database time and reads {} \
             pages a run to return {}: an index on them would find its rows directly. ",
            query_handle(h.query),
            h.query.id,
            format::percent(h.share),
            format::count(h.pages),
            if h.rows <= 0.0 { "nothing" } else { "less than a row" }
        ),
        (Some(s), None) => format!(
            "Azure scores it {s:.0} out of 100: it expects the index to save little next to what it costs every \
             write, so it's usually not worth having. "
        ),
        (None, _) => String::new(),
    };
    // Low on Azure's word alone: the advice is conditional, and so is checking it.
    let conditional = low.is_some() && heavy.is_none();
    let target = qualified(Engine::Mssql, &ix.schema, &ix.table);
    let name = ix.name.clone().unwrap_or_else(|| format!("ix_{}_{}", ix.table, ix.columns.join("_")));
    let create = |why: Option<String>| Fix {
        summary: format!(
            "{}{rated}{}",
            why.map(|w| format!("{w} ")).unwrap_or_default(),
            if conditional {
                format!(
                    "If the queries that read {} are slow all the same, create it online, outside the busiest hours.",
                    ix.table
                )
            } else if low.is_some() {
                "Create it online, outside the busiest hours.".to_string()
            } else {
                "Create it online, outside the busiest hours. Azure SQL can also create recommended indexes itself \
                 (automatic tuning's CREATE_INDEX) and drops one that doesn't help."
                    .to_string()
            }
        ),
        // Azure's own script, unless it includes a column left out above.
        sql: Some(r.script.clone().filter(|_| large.is_empty()).unwrap_or_else(|| {
            format!(
                "CREATE NONCLUSTERED INDEX {} ON {target} ({}){} WITH (ONLINE = ON);",
                quote(Engine::Mssql, &name),
                ix.columns.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", "),
                if include.is_empty() {
                    String::new()
                } else {
                    format!(
                        " INCLUDE ({})",
                        include.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", ")
                    )
                }
            )
        })),
        cost: None,
    };
    // The condition a low score leaves ("if the queries are slow all the same") is one the scan can answer: where every
    // statement behind the recommendation runs fast on average, nothing is slow for the index to speed up.
    let fast = (conditional && same_keys.is_none() && picked.is_none() && !filtering.is_empty())
        .then(|| {
            filtering.iter().map(|q| q.mean_time_ms).fold((f64::MAX, 0.0f64), |(lo, hi), t| (lo.min(t), hi.max(t)))
        })
        .filter(|(_, slowest)| *slowest < SLOW_RUN_MS);
    let fix = match (same_keys, picked.as_ref().filter(|_| low.is_some())) {
        (None, None) if let Some((fastest, slowest)) = fast => {
            let average = if format::duration(fastest) == format::duration(slowest) {
                format::duration(slowest)
            } else {
                format!("{}–{}", format::duration(fastest), format::duration(slowest))
            };
            let statements = match filtering.len() {
                1 => format!(
                    "the statement this scan captured that compares {keys} with a value averages {average} a run"
                ),
                n => format!(
                    "the {n} statements this scan captured that compare {keys} with a value average {average} a run"
                ),
            };
            // A key of two values (a bit, a boolean) splits the table in two at most: it narrows little.
            let bits = ix.columns.iter().all(|c| {
                snapshot.columns.iter().any(|col| {
                    col.schema.eq_ignore_ascii_case(&ix.schema)
                        && col.table.eq_ignore_ascii_case(&ix.table)
                        && col.name.eq_ignore_ascii_case(c)
                        && matches!(col.data_type.to_ascii_lowercase().as_str(), "bit" | "boolean" | "bool")
                })
            });
            let narrows = if bits {
                format!(
                    ", and {}, {}: {} at most two values, so an index on {} narrows the rows little",
                    if ix.columns.len() == 1 { "its key" } else { "its keys" },
                    if ix.columns.len() == 1 {
                        format!("{columns}, is a bit column")
                    } else {
                        format!("{columns}, are bit columns")
                    },
                    if ix.columns.len() == 1 { "it holds" } else { "each holds" },
                    if ix.columns.len() == 1 { "it" } else { "them" }
                )
            } else {
                String::new()
            };
            let copies = if ix.include.is_empty() {
                String::new()
            } else {
                format!(
                    "; every write that changes one of the {} columns it includes would update it too",
                    ix.include.len()
                )
            };
            Fix {
                summary: format!(
                    "{rated}Nothing to create: {statements}, so there is no slow read for it to speed up{narrows}{copies}. Let the recommendation expire."
                ),
                sql: None,
                cost: None,
            }
        }
        (Some(existing), _) => {
            // What the index really leads with: its own key order, which needn't be the recommendation's.
            let leads = format::list(
                &existing.key_names()[..ix.columns.len()].iter().map(|k| k.to_string()).collect::<Vec<_>>(),
            );
            // The fix says what the existing index has and what the recommendation would add: no caveat repeats it.
            if lacking.is_empty() {
                Fix {
                    summary: format!(
                        "{rated}Nothing to create: {} already finds these rows by {leads}, and the recommended index \
                         would only copy {} into a second index. Let the recommendation expire.",
                        existing.name,
                        format::list(&large)
                    ),
                    sql: None,
                    cost: None,
                }
            } else {
                let them = if lacking.len() == 1 { "it" } else { "them" };
                f.caveats.push(format!(
                    "{} already leads with {leads}; the recommended index adds {}, which spares the queries that read \
                     {them} the lookups back to the table. Including {them} in {} does the same with one index fewer to \
                     maintain.",
                    existing.name,
                    format::list(&lacking),
                    existing.name
                ));
                match crate::indexes::widen_index(snapshot, existing, &lacking) {
                    crate::indexes::Widening::Sql(fix) => Fix { summary: format!("{rated}{}", fix.summary), ..fix },
                    crate::indexes::Widening::Refused(why) => create(Some(why)),
                }
            }
        }
        // Azure's low score, and the one statement that would say otherwise reads past an index there is: nothing to
        // create, and the heavy-query finding has the fix.
        (None, Some(p)) => Fix {
            summary: format!(
                "{rated}Nothing to create: {} already leads with {}, which query {} compares with a value, but its \
                 plan picks {} expecting {}, and its runs read {} pages each to return {}. The recorded pages do not \
                 establish why that plan was chosen: inspect a representative actual plan before choosing a change \
                 (see heavy-query:{}). Let the recommendation expire.",
                p.other.name,
                format::list(&p.keys),
                p.query.id,
                p.used,
                rows_expected(p.estimate),
                format::count(p.pages),
                if p.rows <= 0.0 { "nothing" } else { "less than a row" },
                p.query.id
            ),
            sql: None,
            cost: None,
        },
        (None, None) => {
            if let Some((other, keys)) = leading_with_some(snapshot, ix) {
                // The heavy statement's plan says which index it reads the table through: the engine's own choice.
                let instead = heavy.as_ref().and_then(|h| {
                    let used = indexes_read_through(h.query, &ix.schema, &ix.table);
                    (!used.is_empty() && !used.iter().any(|u| u.eq_ignore_ascii_case(&other.name)))
                        .then(|| (h.query.id.clone(), format::list(&used)))
                });
                let check = match (instead, &picked) {
                    (Some((id, used)), _) => format!(
                        ", but query {id}'s plan reads {} through {used} instead, so that index isn't serving it.",
                        ix.table
                    ),
                    (None, Some(p)) => format!(
                        ", but query {}'s plan reads {} through {} instead, expecting {}: heavy-query:{} is about \
                         that choice.",
                        p.query.id,
                        ix.table,
                        p.used,
                        rows_expected(p.estimate),
                        p.query.id
                    ),
                    (None, None) => format!(
                        ": check whether the queries that read {} use it before adding an index beside it.",
                        ix.table
                    ),
                };
                f.caveats.push(format!(
                    "{} already leads with {}{}, {} of the {} recommended keys{check}",
                    other.name,
                    format::list(&keys),
                    other
                        .filter
                        .as_deref()
                        .map(|filter| format!(" for the rows where {}", crate::indexes::readable_filter(filter)))
                        .unwrap_or_default(),
                    keys.len(),
                    ix.columns.len(),
                ));
            }
            create(None)
        }
    };
    // Columns left out of INCLUDE matter only for an index worth creating.
    if let Some(note) = large_note.filter(|_| fix.sql.is_some()) {
        f.caveats.push(note);
    }
    let nothing_to_create = fix.sql.is_none() && ((same_keys.is_some() && lacking.is_empty()) || fast.is_some());
    let picked_nothing = fix.sql.is_none() && same_keys.is_none();
    if fix.sql.is_none() {
        f.informational();
    }
    f.fix = Some(fix);
    let fewer_reads = match &heavy {
        Some(h) => format!("query {} reads far fewer than {} pages a run", h.query.id, format::count(h.pages)),
        None => format!("the queries that read {} do fewer reads", ix.table),
    };
    f.verify = vec![if nothing_to_create {
        "Nothing to change: once Azure lets the recommendation expire, this finding goes with it.".to_string()
    } else if let Some(p) = picked.as_ref().filter(|_| picked_nothing) {
        format!(
            "Nothing to create here: gather the actual-plan and IO evidence described in heavy-query:{} first. \
             After a change supported by that evidence, compare equivalent runs of query {} for lower reads and \
             time per run. When Azure lets the recommendation expire, this finding goes with it.",
            p.query.id, p.query.id
        )
    } else if conditional {
        format!("Only if you apply it: re-scan a day later; the recommendation is gone, and {fewer_reads}.")
    } else {
        format!("Re-scan a day later: the recommendation is gone, and {fewer_reads}.")
    }];
    Some(f)
}

/// The heavy-query check's bar: a statement that takes this share of database time is one of the heaviest.
const HEAVY_SHARE: f64 = 0.05;

/// Pages a run from which a read that returns nothing is reading for nothing (the heavy-query check's own bar).
const NOTHING_READS: f64 = 100.0;

/// The statements the scan captured that compare every key of a recommended index with a value (`tenant_id = @p0 AND
/// is_deleted = 0`): those the index would find rows for by its keys. Not monitoring or session statements.
fn statements_filtering_on<'a>(snapshot: &'a Snapshot, ix: &RecommendedIndex) -> Vec<&'a QueryStat> {
    let value = |v: &str| (v.len() > 1 && v.starts_with('@')) || crate::indexes::is_literal(v) || v.starts_with("n'");
    let mut found: Vec<&QueryStat> = queries_touching(snapshot, &ix.schema, &ix.table)
        .into_iter()
        .filter(|q| !is_monitoring_query(q) && !culprit_core::sql::is_session_statement(&q.text))
        .filter(|q| ix.columns.iter().all(|c| crate::indexes::compared_to(&q.text, &ix.table, c, value)))
        .collect();
    found.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    found
}

/// A statement behind a recommendation measured heavy and reading for nothing.
struct HeavyReader<'a> {
    query: &'a QueryStat,
    share: f64,
    pages: f64,
    rows: f64,
}

/// Of the statements behind a recommendation, the heaviest that takes a heavy query's share of database time and
/// reads at least [`NOTHING_READS`] pages a run to return less than a row: its filter keeps nothing of what it reads, so
/// an index on its keys finds its rows directly (the share kept is far under `INDEX_KEEPS_AT_MOST`). What the runs
/// measured outweighs Azure's estimate.
fn heavy_reader<'a>(snapshot: &Snapshot, statements: &[&'a QueryStat]) -> Option<HeavyReader<'a>> {
    let workload = snapshot.workload.as_ref()?;
    statements.iter().find_map(|&q| {
        // A heavy query's share as every surface states it (`Workload::statement_share`).
        let share = workload.statement_share(q);
        let (pages, rows) = (q.reads_per_execution()?, q.rows_per_execution()?);
        (!q.writes_rows() && share >= HEAVY_SHARE && pages >= NOTHING_READS && rows < 1.0).then_some(HeavyReader {
            query: q,
            share,
            pages,
            rows,
        })
    })
}

/// A heavy statement behind a recommendation whose plan reads the table through `used`, picked on an estimate of
/// `estimate` rows, while `other` already leads with `keys`, columns it compares with a value.
struct PickedElsewhere<'a> {
    query: &'a QueryStat,
    other: &'a IndexStat,
    keys: Vec<String>,
    used: String,
    estimate: f64,
    pages: f64,
    rows: f64,
}

/// A heavy statement with many recorded pages beside a seek estimated at few rows, while another index leads with a
/// key it compares with a value and the seek doesn't seek on. That index can serve the predicate (unfiltered, or its
/// filter as the statement writes it); the page count proves no row misestimate or reason for the choice. The
/// heavy-query finding asks for actual measurements. `None` without a plan or a seek on the table.
fn picked_elsewhere<'a>(
    snapshot: &'a Snapshot,
    ix: &RecommendedIndex,
    h: &HeavyReader<'a>,
) -> Option<PickedElsewhere<'a>> {
    let seek = first_seek(h.query, &ix.schema, &ix.table)?;
    let used = seek.index.as_deref()?.trim_matches(['[', ']', '"']).to_string();
    let estimate = seek.est_rows_total();
    if estimate * culprit_core::MISESTIMATE_FACTOR > h.pages {
        return None;
    }
    let sought = predicate_columns(seek.seek_predicate.as_deref().unwrap_or_default());
    let is_sought = |c: &str| sought.equality.iter().chain(&sought.range).any(|s| s.eq_ignore_ascii_case(c));
    let recommended = |c: &str| ix.columns.iter().any(|k| k.eq_ignore_ascii_case(c));
    snapshot
        .indexes
        .iter()
        .filter(|i| i.schema.eq_ignore_ascii_case(&ix.schema) && i.table.eq_ignore_ascii_case(&ix.table))
        .filter(|i| !i.is_disabled && !i.name.eq_ignore_ascii_case(&used))
        .filter(|i| crate::indexes::fits_index_filter(i, &[h.query]))
        .filter(|i| i.key_names().first().is_some_and(|k| recommended(k) && !is_sought(k)))
        .map(|i| {
            let keys: Vec<String> =
                i.key_names().into_iter().take_while(|k| recommended(k)).map(str::to_string).collect();
            (i, keys)
        })
        .max_by_key(|(i, keys)| (keys.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len())))
        .map(|(other, keys)| PickedElsewhere {
            query: h.query,
            other,
            keys,
            used,
            estimate,
            pages: h.pages,
            rows: h.rows,
        })
}

/// "1 row", "12 rows": what a plan expects a read to find.
fn rows_expected(rows: f64) -> String {
    let rows = if rows >= 10.0 { rows.round() } else { (rows * 10.0).round() / 10.0 };
    format!("{} {}", format::count(rows), if rows == 1.0 { "row" } else { "rows" })
}

/// The first step of a statement's captured plan on a table, when it seeks an index.
fn first_seek<'a>(q: &'a QueryStat, schema: &str, table: &str) -> Option<&'a culprit_core::PlanNode> {
    q.plan
        .as_ref()?
        .nodes()
        .into_iter()
        .find(|n| {
            n.object
                .as_deref()
                .and_then(split_object)
                .is_some_and(|(s, t)| s.eq_ignore_ascii_case(schema) && t.eq_ignore_ascii_case(table))
                && matches!(n.kind, NodeKind::Seek | NodeKind::Scan)
        })
        .filter(|n| n.kind == NodeKind::Seek && n.index.is_some())
}

/// The indexes a statement's captured plan seeks or scans on a table, in plan order (not the lookups back to it): how
/// the optimizer chose to read it. Empty without a plan.
fn indexes_read_through(q: &QueryStat, schema: &str, table: &str) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    for n in q.plan.as_ref().map(|p| p.nodes()).unwrap_or_default() {
        let on_table = n
            .object
            .as_deref()
            .and_then(split_object)
            .is_some_and(|(s, t)| s.eq_ignore_ascii_case(schema) && t.eq_ignore_ascii_case(table));
        if let (true, NodeKind::Seek | NodeKind::Scan, Some(index)) = (on_table, n.kind, n.index.as_deref())
            && !used.iter().any(|u| u.eq_ignore_ascii_case(index))
        {
            used.push(index.to_string());
        }
    }
    used
}

/// The enabled index on the recommendation's table that leads with the most of its keys, in any order, when one leads
/// with any: the queries behind the recommendation may already use it. The unfiltered one first, then the narrowest.
fn leading_with_some<'a>(snapshot: &'a Snapshot, ix: &RecommendedIndex) -> Option<(&'a IndexStat, Vec<String>)> {
    snapshot
        .indexes
        .iter()
        .filter(|i| {
            i.schema.eq_ignore_ascii_case(&ix.schema) && i.table.eq_ignore_ascii_case(&ix.table) && !i.is_disabled
        })
        .filter_map(|i| {
            let keys: Vec<String> = i
                .key_names()
                .into_iter()
                .take_while(|k| ix.columns.iter().any(|c| c.eq_ignore_ascii_case(k)))
                .map(str::to_string)
                .collect();
            (!keys.is_empty()).then_some((i, keys))
        })
        .max_by_key(|(i, keys)| (keys.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len())))
}

pub(super) fn drop_index_advice(
    snapshot: &Snapshot,
    r: &TuningRecommendation,
    ix: &RecommendedIndex,
) -> Option<Finding> {
    let name = ix.name.as_deref()?;
    // Already gone, or the unused- and duplicate-index findings already say to drop it.
    let existing = snapshot.indexes.iter().find(|i| {
        i.schema.eq_ignore_ascii_case(&ix.schema)
            && i.table.eq_ignore_ascii_case(&ix.table)
            && i.name.eq_ignore_ascii_case(name)
    })?;
    if is_unused(snapshot, existing) || redundant_indexes(snapshot).iter().any(|(a, _)| std::ptr::eq(*a, existing)) {
        return None;
    }
    let table_name = format!("{}.{}", ix.schema, ix.table);
    let mut f = finding(
        "tuning-advice",
        &format!("drop-index:{table_name}.{}", existing.name),
        Category::Indexes,
        subject(SubjectKind::Index, format!("{table_name}.{}", existing.name)),
    );
    f.title = format!("Azure SQL recommends dropping {} on {}", existing.name, ix.table);
    f.summary = format!(
        "Azure SQL Database's automatic tuning recommends dropping {}{}{}, judging from a longer history than the usage \
         counters here{}.",
        existing.name,
        r.reason.as_deref().filter(|s| !s.is_empty()).map(|s| format!(" ({s})")).unwrap_or_default(),
        r.valid_since
            .as_deref()
            .map(|s| format!(" since {}", format::friendly_date(s, &snapshot.taken_at)))
            .unwrap_or_default(),
        existing
            .reads
            .map(|reads| format!(", which count {} reads of it {}", format::count(reads), since_phrase(snapshot)))
            .unwrap_or_default()
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + r.score.unwrap_or(0.0) / 20.0;
    f.impact.label = "Recommended by Azure SQL".into();
    f.confidence = Confidence::Medium;
    if let Some(size) = existing.size_bytes {
        f.metrics.push(Metric::new("Size", size, Unit::Bytes));
    }
    if let Some(reads) = existing.reads {
        f.metrics.push(Metric::new("Reads", reads, Unit::Count));
    }
    let target = qualified(Engine::Mssql, &existing.schema, &existing.table);
    f.fix = Some(Fix {
        summary:
            "Disable it first: the definition stays, nothing reads or maintains it, and a rebuild brings it back. \
            Drop it after a full business cycle without complaints."
                .into(),
        sql: Some(format!("ALTER INDEX {} ON {target} DISABLE;", quote(Engine::Mssql, &existing.name))),
        cost: None,
    });
    if let Some(definition) = &existing.definition {
        f.caveats.push(format!("Its definition, to recreate it:\n{definition}"));
    }
    f.verify = vec![format!("Watch the queries on {} for a week after disabling; none should get slower.", ix.table)];
    Some(f)
}
