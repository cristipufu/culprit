//! Plan enrichment boundary: stamps catalog facts (changed column types, table indexes, partition parents, index
//! access methods, MySQL possible keys, and SQL Server parallelism settings) onto plans from a [`Snapshot`].

use std::collections::HashMap;

use crate::format;
use crate::model::{ColumnInfo, Engine, IndexStat, Snapshot};
use crate::plan::{NodeKind, Plan, PlanNode};
use crate::sql::identifier_parts;

use super::predicates::{column_reference, convert_calls, is_column_operand, same_type};
use super::{STATISTICS_NOTE, conditions, is_system_object, table_leaf};

const STATISTICS_MAINTENANCE: &str = "Statistics maintenance";

/// Plan prop: columns this plan converts that the catalog says already have the type it converts them to, one
/// `table.column type` a line. The plan was captured before they changed: Query Store keeps a plan's first XML when a
/// recompile after the schema change keeps the plan's shape, so its text can describe types the table no longer has.
pub const CHANGED_SINCE_CAPTURE: &str = "Columns changed since capture";

/// The row count a statement's LIMIT asked for, on a plan whose statement text keeps a placeholder for it (MySQL's
/// digest `LIMIT ?`): a provider that planned a sample with its values records the sample's count here, so rows
/// returned under the LIMIT can be weighed against it.
pub const LIMIT_ROWS: &str = "LIMIT rows (sample)";

/// Where [`mark_table_indexes`] keeps the indexes the catalog lists on the table a scan reads: `name=col,col` per index,
/// one per line, empty when it has none. Absent where the snapshot doesn't say (no catalog, or a name it can't place).
pub const TABLE_INDEXES: &str = "Indexes on the table (catalog)";

/// The SQL Server settings that decide whether a plan can go parallel, as the snapshot read them, stamped on each of
/// its plans: the server's cost threshold for parallelism, and MAXDOP for the server and for the database (0: as many
/// CPUs as there are).
pub const COST_THRESHOLD: &str = "Cost threshold for parallelism (server, now)";
pub const SERVER_MAXDOP: &str = "Max degree of parallelism (server, now)";
pub const DATABASE_MAXDOP: &str = "MAXDOP (database, now)";

/// Where [`mark_table_indexes`] marks an operator whose table the catalog no longer lists in any schema: it was dropped
/// or renamed since the plan was captured, so there is nothing to index.
pub const TABLE_GONE: &str = "Table not in the catalog";

/// Where [`mark_possible_keys`] keeps each possible key's columns: `name=col,col` per key, one per line.
const POSSIBLE_KEY_COLUMNS: &str = "possible_keys_columns";

pub(super) fn updates_statistics(plan: &Plan) -> bool {
    plan.engine == Engine::Mssql
        && (plan.facts().statistics_maintenance.is_some()
            || plan.statement.as_deref().and_then(|s| statistics_maintenance_note(plan.engine, s)).is_some())
}

/// The recorded operation is authoritative when SQL Server captures its internal SELECT/sort instead of the
/// enclosing UPDATE STATISTICS. A user SELECT with a similar shape establishes no maintenance operation.
pub fn statistics_maintenance_note(engine: Engine, statement: &str) -> Option<&'static str> {
    (engine == Engine::Mssql && crate::sql::mssql_statistics_target(statement).is_some()).then_some(STATISTICS_NOTE)
}

pub fn mark_query(plan: &mut Plan, statement: &str) {
    plan.facts_mut().statistics_maintenance = None;
    plan.props.remove(STATISTICS_MAINTENANCE);
    if statistics_maintenance_note(plan.engine, statement).is_some() {
        plan.facts_mut().statistics_maintenance = Some("UPDATE STATISTICS".into());
        plan.props.insert(STATISTICS_MAINTENANCE.into(), "UPDATE STATISTICS".into());
    }
}

/// Recompute catalog-backed conversion context. A qualified operand never inherits another schema's column type.
pub fn mark_changed_columns(plan: &mut Plan, columns: &[ColumnInfo]) {
    if plan.engine != Engine::Mssql {
        return;
    }
    let mut texts: Vec<String> = plan.facts().conversions.iter().map(|c| c.expression.clone()).collect();
    texts.extend(plan.nodes().into_iter().map(conditions));
    let mut changed: Vec<crate::PlanChangedColumn> = Vec::new();
    for text in &texts {
        for (target, operand) in convert_calls(text).into_iter().filter(|(_, o)| is_column_operand(o)) {
            let Some(reference) = column_reference(plan, operand) else { continue };
            let Some(table) = reference.table.as_deref() else { continue };
            let mut candidates = columns.iter().filter(|c| {
                c.table.eq_ignore_ascii_case(table)
                    && c.name.eq_ignore_ascii_case(&reference.column)
                    && reference.schema.as_deref().is_none_or(|s| c.schema.eq_ignore_ascii_case(s))
            });
            let (Some(now), None) = (candidates.next(), candidates.next()) else { continue };
            if same_type(&now.data_type, target) {
                let fact = crate::PlanChangedColumn {
                    schema: Some(now.schema.clone()),
                    table: now.table.clone(),
                    column: now.name.clone(),
                    data_type: now.data_type.clone(),
                };
                if !changed.contains(&fact) {
                    changed.push(fact);
                }
            }
        }
    }
    plan.props.remove(CHANGED_SINCE_CAPTURE);
    if !changed.is_empty() {
        plan.props.insert(
            CHANGED_SINCE_CAPTURE.into(),
            changed.iter().map(|c| format!("{}.{} {}", c.table, c.column, c.data_type)).collect::<Vec<_>>().join("\n"),
        );
    }
    plan.facts_mut().changed_columns = changed;
}

/// `mark_changed_columns` on every plan of the snapshot's workload, against the snapshot's own catalog; the indexes on
/// each table a plan scans ([`mark_table_indexes`]); and, on SQL Server, the settings that decide whether a plan can go
/// parallel ([`mark_parallel_settings`]).
pub fn mark_snapshot(snapshot: &mut Snapshot) {
    let parallel = parallel_settings(snapshot);
    let tables = scanned_table_indexes(snapshot);
    let columns = &snapshot.columns;
    let partitions = partition_parents(&snapshot.tables);
    if let Some(workload) = snapshot.workload.as_mut() {
        let end = workload.window_end.as_deref().and_then(format::epoch_seconds);
        for q in workload.queries.iter_mut() {
            // A table missing from the catalog is gone only for a statement that stopped: a dropped table stops the
            // statements that read it, while one the catalog doesn't list (an indexed view) runs on.
            let quiet = match (end, q.last_seen.as_deref().and_then(format::epoch_seconds)) {
                (Some(end), Some(last)) => end - last >= 86_400.0,
                _ => false,
            };
            let Some(plan) = q.plan.as_mut() else { continue };
            mark_query(plan, &q.text);
            mark_changed_columns(plan, columns);
            mark_partitions(&mut plan.root, &partitions);
            mark_table_indexes(&mut plan.root, &tables, quiet);
            if plan.engine == Engine::Mssql {
                mark_parallel_settings(plan, &parallel);
            }
        }
    }
}

/// [`mark_snapshot`]'s catalog marks on one plan fetched apart from a scan (a stored plan opened from a report): the
/// indexes on each table it scans, changed column types, possible keys, index kinds and parallelism settings.
/// Not [`TABLE_GONE`], which needs to know that the statement stopped running.
pub fn mark_plan(plan: &mut Plan, snapshot: &Snapshot) {
    if let Some(statement) = plan.statement.clone() {
        mark_query(plan, &statement);
    }
    mark_changed_columns(plan, &snapshot.columns);
    mark_partitions(&mut plan.root, &partition_parents(&snapshot.tables));
    mark_table_indexes(&mut plan.root, &scanned_table_indexes(snapshot), false);
    if plan.engine == Engine::Mssql {
        mark_parallel_settings(plan, &parallel_settings(snapshot));
    }
    mark_possible_keys(plan, &snapshot.indexes);
    mark_index_kinds(&mut plan.root, &snapshot.indexes, None);
}

fn partition_parents(tables: &[crate::TableStat]) -> HashMap<String, Option<String>> {
    let mut parents = HashMap::new();
    for t in tables {
        // An unquoted display name can be ambiguous when an identifier itself contains a dot.
        // Do not attribute such an operator to either of two different catalog objects.
        parents.entry(t.qualified()).and_modify(|parent| *parent = None).or_insert_with(|| t.partition_of.clone());
    }
    parents
}

fn mark_partitions(node: &mut PlanNode, parents: &HashMap<String, Option<String>>) {
    node.facts_mut().partition_parent = None;
    node.props.remove("Partition Of");
    if let Some(parent) = node.object.as_deref().and_then(|name| parents.get(name)).and_then(Option::as_ref) {
        node.facts_mut().partition_parent = Some(parent.clone());
        node.props.insert("Partition Of".into(), parent.clone());
    }
    for child in &mut node.children {
        mark_partitions(child, parents);
    }
}

fn parallel_settings(snapshot: &Snapshot) -> Vec<(&'static str, String)> {
    let setting = |name: &str, scope: &str| {
        snapshot
            .settings
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name) && s.scope.eq_ignore_ascii_case(scope))
            .map(|s| s.value.trim().to_string())
            .filter(|v| v.parse::<f64>().is_ok())
    };
    [
        (COST_THRESHOLD, setting("cost threshold for parallelism", "server")),
        (SERVER_MAXDOP, setting("max degree of parallelism", "server")),
        (DATABASE_MAXDOP, setting("MAXDOP", "database")),
    ]
    .into_iter()
    .filter_map(|(key, value)| Some((key, value?)))
    .collect()
}

/// Stamps the server's parallelism settings ([`COST_THRESHOLD`], [`SERVER_MAXDOP`], [`DATABASE_MAXDOP`]) on a plan.
pub fn mark_parallel_settings(plan: &mut Plan, settings: &[(&'static str, String)]) {
    use crate::plan_facts::{count, nonnegative};
    let facts = plan.facts_mut();
    facts.cost_threshold_for_parallelism = None;
    facts.server_maxdop = None;
    facts.database_maxdop = None;
    for key in [COST_THRESHOLD, SERVER_MAXDOP, DATABASE_MAXDOP] {
        plan.props.remove(key);
    }
    for (key, value) in settings {
        let number = value.trim().parse::<f64>().ok().and_then(nonnegative);
        match *key {
            COST_THRESHOLD => plan.facts_mut().cost_threshold_for_parallelism = number,
            SERVER_MAXDOP => plan.facts_mut().server_maxdop = number.and_then(count),
            DATABASE_MAXDOP => plan.facts_mut().database_maxdop = number.and_then(count),
            _ => {}
        }
        plan.props.insert((*key).to_string(), value.clone());
    }
}

/// Each table of the catalog (lower-cased `schema`, `name`) with its enabled indexes as [`TABLE_INDEXES`] writes them.
/// Empty without a catalog of indexes, which would make every table look like it has none.
#[derive(Clone)]
pub(super) struct CatalogTable {
    pub(super) schema: String,
    pub(super) name: String,
    pub(super) indexes: Vec<crate::PlanIndexColumns>,
}

fn scanned_table_indexes(snapshot: &Snapshot) -> Vec<CatalogTable> {
    if snapshot.indexes.is_empty() {
        return Vec::new();
    }
    snapshot
        .tables
        .iter()
        .map(|t| CatalogTable {
            schema: t.schema.clone(),
            name: t.name.clone(),
            indexes: snapshot
                .indexes
                .iter()
                .filter(|i| {
                    i.schema.eq_ignore_ascii_case(&t.schema) && i.table.eq_ignore_ascii_case(&t.name) && !i.is_disabled
                })
                .map(|i| crate::PlanIndexColumns {
                    name: i.name.clone(),
                    columns: i.key_names().into_iter().map(str::to_string).collect(),
                })
                .collect(),
        })
        .collect()
}

/// Match a name to one catalog candidate. Callers must reject multiple matches: an unqualified name does not
/// establish a schema, and an unquoted display name can be ambiguous when identifiers themselves contain dots.
pub fn catalog_table_matches(object: &str, schema: &str, table: &str) -> bool {
    if object.eq_ignore_ascii_case(&format!("{schema}.{table}")) || object.eq_ignore_ascii_case(table) {
        return true;
    }
    let Some(parts) = identifier_parts(object) else { return false };
    match parts.as_slice() {
        [name] => name.eq_ignore_ascii_case(table),
        [s, name] => s.eq_ignore_ascii_case(schema) && name.eq_ignore_ascii_case(table),
        _ => false,
    }
}

/// Marks every scan of a table with the indexes the catalog lists on it ([`TABLE_INDEXES`]), where the scan's object
/// names one table of the catalog: by its schema when the plan says it, else by a name only one schema has. In the plan
/// of a statement that stopped running (`stopped`), an operator on a table the catalog lists in no schema is marked
/// [`TABLE_GONE`]; not one the plan names without a schema (SQLite names a table by its alias) or a derived table.
pub(super) fn mark_table_indexes(n: &mut PlanNode, tables: &[CatalogTable], stopped: bool) {
    n.facts_mut().table_indexes = None;
    n.facts_mut().table_missing = None;
    n.props.remove(TABLE_INDEXES);
    n.props.remove(TABLE_GONE);
    if let Some(object) = n.object.clone().filter(|o| !o.starts_with(['@', '#']) && !tables.is_empty()) {
        let mut candidates = tables.iter().filter(|t| catalog_table_matches(&object, &t.schema, &t.name));
        let first = candidates.next();
        if let (Some(table), None) = (first, candidates.next()) {
            if n.kind == NodeKind::Scan {
                let indexes = table.indexes.clone();
                n.props.insert(
                    TABLE_INDEXES.into(),
                    indexes
                        .iter()
                        .map(|i| format!("{}={}", i.name, i.columns.join(",")))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                n.facts_mut().table_indexes = Some(indexes);
            }
        } else if first.is_none()
            && stopped
            && identifier_parts(&object).is_some_and(|p| p.len() == 2)
            && !object.starts_with('<')
            && !is_system_object(&object)
        {
            n.facts_mut().table_missing = Some(object.clone());
            n.props.insert(TABLE_GONE.into(), object);
        }
    }
    for child in &mut n.children {
        mark_table_indexes(child, tables, stopped);
    }
}

pub(super) fn table_indexes(n: &PlanNode) -> Option<Vec<(String, Vec<String>)>> {
    n.facts().table_indexes.as_ref().map(|v| v.iter().map(|i| (i.name.clone(), i.columns.clone())).collect())
}

/// MySQL names a scan's possible keys without their columns. Name each with its key columns from the catalog, so an
/// index on the filtered column can be told from one on the join column ([`super::predicates::possible_keys_for_filter`]).
/// A key the catalog doesn't show, or shows on two tables of that name, leaves the scan unmarked.
pub fn mark_possible_keys(plan: &mut Plan, indexes: &[IndexStat]) {
    if plan.engine != Engine::Mysql {
        return;
    }
    fn walk(n: &mut PlanNode, indexes: &[IndexStat]) {
        n.facts_mut().possible_index_columns = None;
        n.props.remove(POSSIBLE_KEY_COLUMNS);
        if let Some(object) = n.object.clone() {
            let described: Option<Vec<crate::PlanIndexColumns>> = n
                .facts()
                .possible_index_names
                .iter()
                .map(|key| {
                    let mut found = indexes.iter().filter(|i| {
                        !i.is_disabled
                            && i.name.eq_ignore_ascii_case(key)
                            && catalog_table_matches(&object, &i.schema, &i.table)
                    });
                    let (Some(index), None) = (found.next(), found.next()) else { return None };
                    Some(crate::PlanIndexColumns {
                        name: key.clone(),
                        columns: index.key_names().into_iter().map(str::to_string).collect(),
                    })
                })
                .collect();
            if let Some(described) = described.filter(|v| !v.is_empty()) {
                n.props.insert(
                    POSSIBLE_KEY_COLUMNS.into(),
                    described
                        .iter()
                        .map(|i| format!("{}={}", i.name, i.columns.join(",")))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                n.facts_mut().possible_index_columns = Some(described);
            }
        }
        for child in &mut n.children {
            walk(child, indexes);
        }
    }
    walk(&mut plan.root, indexes);
}

pub fn mark_snapshot_index_kinds(snapshot: &mut Snapshot) {
    let indexes = &snapshot.indexes;
    let Some(workload) = snapshot.workload.as_mut() else { return };
    for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
        mark_index_kinds(&mut plan.root, indexes, None);
    }
}

pub(super) fn mark_index_kinds(n: &mut PlanNode, indexes: &[IndexStat], bitmap_table: Option<&str>) {
    n.facts_mut().index_method = None;
    n.props.remove(crate::plan::INDEX_KIND);
    let object = n.object.as_deref().or(bitmap_table);
    if let Some(index_name) = n.index.as_deref() {
        let index_name = table_leaf(index_name);
        let mut found = indexes.iter().filter(|i| {
            i.name.eq_ignore_ascii_case(&index_name)
                && object.is_none_or(|o| catalog_table_matches(o, &i.schema, &i.table))
        });
        if let (Some(index), None) = (found.next(), found.next())
            && !index.kind.is_empty()
        {
            n.facts_mut().index_method = Some(index.kind.to_ascii_lowercase());
            n.props.insert(crate::plan::INDEX_KIND.into(), index.kind.to_ascii_lowercase());
        }
    }
    let object = n.object.clone().or_else(|| bitmap_table.map(str::to_string));
    let inherited =
        matches!(n.op.as_str(), "Bitmap Heap Scan" | "BitmapAnd" | "BitmapOr").then_some(object.as_deref()).flatten();
    for child in &mut n.children {
        mark_index_kinds(child, indexes, inherited);
    }
}

pub fn mark_snapshot_keys(snapshot: &mut Snapshot) {
    let indexes = &snapshot.indexes;
    if let Some(workload) = snapshot.workload.as_mut() {
        for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
            mark_possible_keys(plan, indexes);
        }
    }
}
