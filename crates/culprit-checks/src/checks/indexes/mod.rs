use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, MissingIndex, NodeKind,
    QueryStat, Severity, Snapshot, SubjectKind, Unit, format,
};

use culprit_core::Check;

use crate::support::*;

mod duplicate_index;
mod filtered_scan;
mod missing_index;
#[cfg(test)]
mod tests;
mod unindexed_fk;
mod unused_index;

pub use duplicate_index::*;
pub use filtered_scan::*;
pub use missing_index::*;
pub use unindexed_fk::*;
pub use unused_index::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] =
    &[&MissingIndexCheck, &FilteredScanCheck, &UnusedIndexCheck, &DuplicateIndexCheck, &UnindexedFkCheck];

pub(crate) const MAX_INCLUDE: usize = 6;

/// What [`widen_index`] can do with an existing index.
pub(crate) enum Widening {
    /// A statement that changes only what was asked (columns included, or the index brought back), with the sentence
    /// that says so and what it costs.
    Sql(Fix),
    /// No statement can do it in place without changing what the index is; the sentence says why, so the caller can
    /// suggest a separate index instead.
    Refused(String),
}

/// The one way Culprit widens an existing index: rebuild it with `include` added to its included columns, keeping
/// everything else it is (unique or not, its filter, its keys in their order and direction, the columns it already
/// includes, and ONLINE where the edition has it), or bring it back when it is disabled. Fix SQL must never damage what
/// exists, so it refuses rather than guess:
///
/// - a clustered index or a primary key can't take included columns (SQL Server refuses to change a constraint's index
///   with DROP_EXISTING, and the clustered index already holds every column);
/// - only SQL Server rebuilds an index with new included columns in place (PostgreSQL would need a new index and
///   MySQL has no INCLUDE).
///
/// A disabled index (SQL Server DISABLE, a PostgreSQL index left invalid by a failed build, a MySQL invisible or
/// MariaDB ignored one) is brought back by a rebuild, or made visible again; on SQL Server the rebuild includes
/// `include` when it can. Used by the missing-index finding, the index advisor and the filtered-scan finding; a plan
/// insight that wants an index to cover a lookup can use it too.
pub(crate) fn widen_index(snapshot: &Snapshot, existing: &IndexStat, include: &[String]) -> Widening {
    let engine = snapshot.server.engine;
    let target = qualified(engine, &existing.schema, &existing.table);
    let name = quote(engine, &existing.name);
    let add = lacking_includes(existing, include);
    let can_include = engine == Engine::Mssql && !existing.is_clustered && !existing.is_primary_key;
    if existing.is_disabled && (add.is_empty() || !can_include) {
        return Widening::Sql(bring_back(snapshot, existing, &add));
    }
    if add.is_empty() {
        return Widening::Refused(format!(
            "{} already has these keys and includes {}.",
            existing.name,
            format::list(include)
        ));
    }
    if existing.is_clustered {
        return Widening::Refused(format!(
            "{} is the table's clustered index: it already holds every column, so there is nothing to include in it.",
            existing.name
        ));
    }
    if existing.is_primary_key {
        return Widening::Refused(format!(
            "{} enforces the primary key, and SQL Server won't change a constraint's index to include columns.",
            existing.name
        ));
    }
    if engine != Engine::Mssql {
        return Widening::Refused(match engine {
            Engine::Postgres => format!(
                "PostgreSQL can't add included columns to {} in place; a new index has to be built.",
                existing.name
            ),
            _ => format!("{} has no included columns: an index there holds only its keys.", snapshot.server.name()),
        });
    }
    let keys: Vec<String> = existing
        .columns
        .iter()
        .map(|k| match k.strip_suffix(" DESC") {
            Some(column) => format!("{} DESC", quote(engine, column)),
            None => quote(engine, k),
        })
        .collect();
    let all_include: Vec<String> = existing.include.iter().chain(&add).map(|c| quote(engine, c)).collect();
    let mut options = vec!["DROP_EXISTING = ON"];
    if supports_online_index(snapshot) {
        options.push("ONLINE = ON");
    }
    // The rebuild replaces the definition whole: without UNIQUE a unique index would stop enforcing it, and without its
    // filter a filtered index would take every row.
    let filter = existing.filter.as_deref().map(|f| format!("\n    WHERE {f}")).unwrap_or_default();
    let sql = format!(
        "CREATE {}NONCLUSTERED INDEX {name}\n    ON {target} ({})\n    INCLUDE ({}){filter}\n    WITH ({});",
        if existing.is_unique { "UNIQUE " } else { "" },
        keys.join(", "),
        all_include.join(", "),
        options.join(", ")
    );
    let mut summary = if existing.is_disabled {
        format!(
            "{} has the right keys but is disabled, so SQL Server neither maintains nor reads it. Rebuilding it brings it \
             back, with {} included so it covers the query; if it was disabled because nothing read it, the queries \
             below now would.",
            existing.name,
            format::list(&add)
        )
    } else {
        format!(
            "{} already has the right keys but makes every matching row go back to the table for {}. Rebuild it with \
             those columns included so it covers the query.",
            existing.name,
            format::list(&add)
        )
    };
    summary.push_str(
        " The statement keeps its keys, their order, its filter and the columns it already includes; restate \
         DATA_COMPRESSION, FILLFACTOR or its partition scheme in it if the index uses them.",
    );
    if existing.is_unique {
        summary.push_str(&format!(
            " It stays unique. If {} enforces a UNIQUE constraint rather than being a unique index, SQL Server refuses \
             the rebuild (error 1907): create a separate index with these keys and included columns instead.",
            existing.name
        ));
    }
    Widening::Sql(Fix {
        summary,
        sql: Some(sql),
        cost: existing.size_bytes.map(|b| {
            format!("The index grows from {}; writes that touch the included columns maintain it.", format::bytes(b))
        }),
    })
}

/// The columns of `include` that `existing` holds neither as a key nor as an included column, once each.
fn lacking_includes(existing: &IndexStat, include: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in include {
        let held = existing.include.iter().any(|i| i.eq_ignore_ascii_case(c))
            || existing.key_names().iter().any(|k| k.eq_ignore_ascii_case(c));
        if !held && !out.iter().any(|o| o.eq_ignore_ascii_case(c)) {
            out.push(c.clone());
        }
    }
    out
}

/// A disabled index brought back as it was: SQL Server rebuilds it, PostgreSQL rebuilds an invalid one, MySQL makes an
/// invisible one visible and MariaDB stops ignoring it. `wanted` are columns a request also wanted included that this
/// can't add.
fn bring_back(snapshot: &Snapshot, existing: &IndexStat, wanted: &[String]) -> Fix {
    let engine = snapshot.server.engine;
    let target = qualified(engine, &existing.schema, &existing.table);
    let name = quote(engine, &existing.name);
    let (sql, how) = match engine {
        Engine::Mssql => {
            // SQL Server can't rebuild a disabled clustered index online.
            let online = supports_online_index(snapshot) && !existing.is_clustered;
            (
                format!("ALTER INDEX {name} ON {target} REBUILD{};", if online { " WITH (ONLINE = ON)" } else { "" }),
                "it is disabled, so SQL Server neither maintains nor reads it. Rebuilding it brings it back as it was",
            )
        }
        Engine::Postgres if culprit_core::version_at_least(&snapshot.server.version, [12, 0, 0]) => (
            format!("REINDEX INDEX CONCURRENTLY {};", qualified(engine, &existing.schema, &existing.name)),
            "it is invalid (a build that didn't finish), so PostgreSQL never reads it. Rebuilding it makes it usable",
        ),
        Engine::Postgres => (
            format!("REINDEX INDEX {};", qualified(engine, &existing.schema, &existing.name)),
            "it is invalid (a build that didn't finish), so PostgreSQL never reads it. Rebuilding it makes it usable \
             (this version can't rebuild concurrently: writes to the table wait meanwhile)",
        ),
        Engine::Mysql if snapshot.server.is_mariadb() => (
            format!("ALTER TABLE {target} ALTER INDEX {name} NOT IGNORED;"),
            "the optimizer is told to ignore it. Letting it use the index again is instant",
        ),
        Engine::Mysql => (
            format!("ALTER TABLE {target} ALTER INDEX {name} VISIBLE;"),
            "it is invisible to the optimizer. Making it visible again is instant",
        ),
        // SQLite has no disabled or invisible indexes: rebuilding is all there is.
        Engine::Sqlite => (
            format!("REINDEX {};", qualified(engine, &existing.schema, &existing.name)),
            "rebuilding it is all SQLite offers",
        ),
    };
    let mut summary = format!(
        "{} has these keys, but {how}; if it was set aside because nothing read it, the queries below now would.",
        existing.name
    );
    if !wanted.is_empty() {
        summary.push_str(&format!(
            " It doesn't include {}; once it's back, the optimizer says whether it still wants {}.",
            format::list(wanted),
            if wanted.len() == 1 { "it" } else { "them" }
        ));
    }
    Fix { summary, sql: Some(sql), cost: None }
}

/// [`widen_index`] as a fix: the statement, or, where none is safe, the sentence that says why and no SQL.
pub(crate) fn widen_fix(snapshot: &Snapshot, existing: &IndexStat, include: &[String]) -> Fix {
    match widen_index(snapshot, existing, include) {
        Widening::Sql(fix) => fix,
        Widening::Refused(why) => Fix { summary: why, sql: None, cost: None },
    }
}

/// Whether a table the statements behind a finding read is gone: the catalog no longer lists it, and they all stopped
/// running (a dropped table stops the statements that read it; one the catalog doesn't list, an indexed view, doesn't).
/// A table of that name in any schema counts as there (a plan can name one without its schema), and so does every table
/// when the scan read no catalog of tables, or no statement is shown.
pub(crate) fn table_gone(snapshot: &Snapshot, table: &str, statements: &[&QueryStat]) -> bool {
    !snapshot.tables.is_empty()
        && !snapshot.tables.iter().any(|t| t.name.eq_ignore_ascii_case(table))
        && !statements.is_empty()
        && statements.iter().all(|q| crate::query_is_history(snapshot, q))
}

/// The fix that creates an index on `keys` (including `include`) of a table for `statements`, with `summary`: the one
/// place an index to add is written. A table that is gone ([`table_gone`]) gets no SQL, which would fail: the fix says
/// so.
pub(crate) fn new_index_fix(
    snapshot: &Snapshot,
    schema: &str,
    table: &str,
    keys: &[String],
    include: &[String],
    statements: &[&QueryStat],
    summary: String,
) -> Fix {
    if table_gone(snapshot, table, statements) {
        return Fix {
            summary: format!(
                "Nothing to create: {schema}.{table} isn't in the database's catalog any more, so it was dropped or \
                 renamed since these statements ran. If they now read a table under another name, a scan of it says \
                 whether that one needs an index."
            ),
            sql: None,
            cost: None,
        };
    }
    Fix {
        summary,
        sql: Some(create_index_sql(snapshot, schema, table, keys, include)),
        cost: write_cost(snapshot, schema, table),
    }
}

/// What an index to add costs the table's writes, for a fix whose SQL creates it ("this index"). A fix that only says
/// to add one, with no SQL, names it [`write_cost_of`]'s way: "this index" is said only beside the SQL that creates it.
pub(crate) fn write_cost(snapshot: &Snapshot, schema: &str, table: &str) -> Option<String> {
    write_cost_of(snapshot, schema, table, "this index")
}

/// [`write_cost`], with the index named as the fix names it: "this index" beside SQL that creates it, "any index you
/// add" for a fix that only says to add one.
pub(crate) fn write_cost_of(snapshot: &Snapshot, schema: &str, table: &str, index: &str) -> Option<String> {
    let (writes, days) = measured_writes(snapshot, schema, table)?;
    // Counters that may have been reset since the server started (performance_schema's, with its statements) cover a
    // span nobody knows: no length is said, and no absence claimed before a week of the statements' own counting.
    let reset = crate::support::counters_may_be_reset(snapshot);
    // No writes says the table gets none only after a week of counters (`watched_long_enough`: on counters that may
    // have been reset, a week of the digests'): a weekly job has had its chance to run.
    if writes <= 0.0 && watched_long_enough(snapshot) {
        return Some(format!(
            "{schema}.{table} received no writes {}, so {index} would cost next to nothing to maintain.",
            since_phrase(snapshot)
        ));
    }
    if writes <= 0.0 && reset {
        return Some(format!(
            "{schema}.{table} had no writes in what its counters cover, which may be short: performance_schema's \
             counters start over when they are reset, and a weekly job may not have run since; each write it gets will \
             also update {index}.",
        ));
    }
    if writes <= 0.0 {
        return Some(format!(
            "{schema}.{table} had no writes in the {} its counters cover, too short to say it gets none (a weekly job \
             may not have run yet); each write it gets will also update {index}.",
            format::duration(days * 86_400_000.0)
        ));
    }
    Some(format!(
        "{schema}.{table} receives {}; each one will also update {index}.",
        how_many(snapshot, writes, days, "write", "writes")
    ))
}

/// Inserts, updates and deletes on a table since its counters started, and the days they cover (at least half an
/// hour, so a just-restarted server doesn't read as a write storm).
pub(crate) fn measured_writes(snapshot: &Snapshot, schema: &str, table: &str) -> Option<(f64, f64)> {
    let base = snapshot.indexes_on(schema, table).find(|i| i.is_clustered || i.is_primary_key).and_then(|i| i.writes);
    let row_writes = snapshot
        .table(schema, table)
        .and_then(|t| Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0)));
    let writes = base.or(row_writes)?;
    let days = counter_days_for_rate(snapshot)?;
    Some((writes, days))
}

/// Never read since the counters started, and not one the database needs for something else: a key, a constraint or
/// the table's storage. A filtered (partial) index counts: unread, it is only upkeep on every write it matches. No
/// reads says something only after a week of watching: an index that may be younger than that, or counters that started
/// less than a week ago (a restart, a failover, a statistics reset), haven't seen a weekly job run yet.
pub(crate) fn is_unused(snapshot: &Snapshot, i: &IndexStat) -> bool {
    let is_candidate = !i.is_primary_key && !i.is_unique && !i.is_clustered && !i.is_disabled && !is_partition_copy(i);
    is_candidate
        && i.reads == Some(0.0)
        && i.size_bytes.unwrap_or(0.0) >= 64.0 * 1024.0
        && !too_new(snapshot, i)
        && watched_long_enough(snapshot)
}

/// Days an index, and the counters watching it, must have existed before no reads says anything about it.
pub(crate) const MIN_INDEX_DAYS: f64 = 7.0;

/// Whether the usage counters have counted for a week ([`MIN_INDEX_DAYS`]). Unknown counts as not. On counters
/// performance_schema may have reset since the server started, the week is the statement digests' own counting
/// ([`crate::support::watched_days`]), never the server's uptime.
pub(crate) fn watched_long_enough(snapshot: &Snapshot) -> bool {
    crate::support::watched_days(snapshot).is_some_and(|d| d >= MIN_INDEX_DAYS)
}

/// Days since the latest it may have been created, when a scan found reason to think it may be new.
fn index_age_days(snapshot: &Snapshot, i: &IndexStat) -> Option<f64> {
    days_between(i.created_at_latest.as_deref()?, &snapshot.taken_at).map(|d| d.max(0.0))
}

/// It may have been created less than a week ago, so its usage counters say little yet.
pub(crate) fn too_new(snapshot: &Snapshot, i: &IndexStat) -> bool {
    index_age_days(snapshot, i).is_some_and(|d| d < MIN_INDEX_DAYS)
}

fn definitions(list: &[&IndexStat]) -> Option<String> {
    let defs: Vec<String> = list.iter().filter_map(|i| i.definition.clone()).collect();
    (!defs.is_empty()).then(|| defs.join(";\n") + ";")
}

pub(crate) fn same_keys(a: &IndexStat, b: &IndexStat) -> bool {
    a.columns.len() == b.columns.len()
        && a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y))
        && same_opclasses(a, b, a.columns.len())
}

/// Whether the first `n` keys of `a` and `b` use the same operator class and collation. On the same column,
/// `text_pattern_ops` (or `COLLATE "C"`) serves `LIKE 'abc%'` and the default serves ORDER BY and range reads: two
/// indexes that differ there serve different queries, and neither duplicates the other.
pub(crate) fn same_opclasses(a: &IndexStat, b: &IndexStat, n: usize) -> bool {
    let class = |i: &IndexStat, k: usize| {
        i.opclasses.as_ref().and_then(|v| v.get(k)).map(|c| c.trim().to_ascii_lowercase()).unwrap_or_default()
    };
    (0..n).all(|k| class(a, k) == class(b, k))
}

/// Whether the index is a partition's copy of a partitioned index (PostgreSQL). Every partition gets one, and a cold
/// partition reads as unused because queries prune it, not because the index is useless; such copies can't be dropped
/// one by one either. They are judged at their parent, never alone.
pub(crate) fn is_partition_copy(i: &IndexStat) -> bool {
    i.partition_of.is_some()
}

/// `b` makes `a` redundant when a's keys are a leading prefix of b's keys and everything a includes is available in b.
/// A hash index finds a whole key only, never a prefix of one: a longer hash index serves none of a shorter one's
/// lookups (only the same keys make one redundant).
fn makes_redundant(b: &IndexStat, a: &IndexStat) -> bool {
    if b.columns.len() < a.columns.len() {
        return false;
    }
    let hash = |i: &IndexStat| i.kind.to_ascii_lowercase().contains("hash");
    if (hash(a) || hash(b)) && b.columns.len() != a.columns.len() {
        return false;
    }
    let prefix = a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y))
        && same_opclasses(a, b, a.columns.len());
    let available: Vec<String> = b.key_names().iter().map(|s| s.to_string()).chain(b.include.iter().cloned()).collect();
    let includes_ok = a.include.iter().all(|c| available.iter().any(|x| x.eq_ignore_ascii_case(c)));
    prefix && includes_ok
}

/// Between two identical indexes, drop the one that is less used, then the one with the later name, so exactly one finding is produced.
fn prefer_drop(a: &IndexStat, b: &IndexStat) -> bool {
    if b.is_unique || b.is_primary_key {
        return true;
    }
    let (ra, rb) = (a.reads.unwrap_or(0.0), b.reads.unwrap_or(0.0));
    if ra != rb {
        return ra < rb;
    }
    a.name > b.name
}

/// Indexes another index makes redundant, mapped to the index that covers them. Shared by the duplicate and unused checks so an index is reported once.
///
/// The keeper is never a clustered index (InnoDB's PRIMARY included, which MySQL reports with the same kind as any
/// other): seeking it reads whole rows, so a narrow index on its leading columns serves reads it can't serve as cheaply.
pub fn redundant_indexes(snapshot: &Snapshot) -> Vec<(&IndexStat, &IndexStat)> {
    let mut out = Vec::new();
    for a in &snapshot.indexes {
        if a.is_primary_key
            || a.is_unique
            || a.is_clustered
            || a.is_disabled
            || a.columns.is_empty()
            || is_partition_copy(a)
        {
            continue;
        }
        let keeper = snapshot
            .indexes_on(&a.schema, &a.table)
            .filter(|b| {
                b.name != a.name && !b.is_disabled && !b.is_clustered && b.filter == a.filter && b.kind == a.kind
            })
            .find(|b| makes_redundant(b, a) && (!same_keys(a, b) || prefer_drop(a, b)))
            // A read index isn't redundant to one nothing reads: drop the unread one instead (the unused check says so).
            .filter(|b| !(is_unused(snapshot, b) && a.reads.is_some_and(|r| r > 0.0)));
        if let Some(b) = keeper {
            out.push((a, b));
        }
    }
    out
}
