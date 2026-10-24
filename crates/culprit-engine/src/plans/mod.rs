//! Plans from outside a connection: pasted text or files saved by other tools. Reads SQL Server showplan XML
//! (.sqlplan), Postgres EXPLAIN as JSON or text, MySQL EXPLAIN FORMAT=JSON (versions 1 and 2) and MySQL's tree output
//! (EXPLAIN ANALYZE, FORMAT=TREE), SQLite's EXPLAIN QUERY PLAN as the sqlite3 shell prints it, plus Culprit's own plan
//! JSON. psql and mysql client decorations are stripped first.

use std::io::Write;
use std::path::PathBuf;

#[cfg(any(feature = "postgres", feature = "mysql", feature = "yugabyte"))]
use culprit_core::PlanKind;
use culprit_core::{Error, Plan, PlanNode, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use ts_rs::TS;

mod pg_text;

/// The largest SQL Server plans run to a few megabytes; anything far beyond that is not a plan.
pub const MAX_PLAN_BYTES: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanFormat {
    ShowplanXml,
    PostgresJson,
    YugabyteJson,
    PostgresText,
    MysqlJson,
    MysqlTree,
    SqliteText,
    Culprit,
}

impl PlanFormat {
    pub fn label(self) -> &'static str {
        match self {
            PlanFormat::ShowplanXml => "SQL Server showplan XML",
            PlanFormat::PostgresJson => "PostgreSQL EXPLAIN (FORMAT JSON)",
            PlanFormat::YugabyteJson => "YugabyteDB EXPLAIN (DIST, FORMAT JSON)",
            PlanFormat::PostgresText => "PostgreSQL EXPLAIN",
            PlanFormat::MysqlJson => "MySQL EXPLAIN FORMAT=JSON",
            PlanFormat::MysqlTree => "MySQL EXPLAIN ANALYZE / FORMAT=TREE",
            PlanFormat::SqliteText => "SQLite EXPLAIN QUERY PLAN",
            PlanFormat::Culprit => "Culprit plan JSON",
        }
    }
}

/// Decode a plan file: UTF-8 (with or without a byte-order mark) or UTF-16 with a byte-order mark, which is how SSMS
/// saves .sqlplan files. A UTF-16 file's XML declaration is rewritten to say UTF-8, which the text now is.
pub fn decode(bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_PLAN_BYTES * 2 {
        return Err(too_large(bytes.len()));
    }
    let utf16 = |units: Vec<u16>| {
        String::from_utf16(&units)
            .map(|s| {
                s.replacen("encoding=\"utf-16\"", "encoding=\"utf-8\"", 1).replacen(
                    "encoding='utf-16'",
                    "encoding='utf-8'",
                    1,
                )
            })
            .map_err(|_| Error::invalid("The file says it is UTF-16 but isn't valid UTF-16"))
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()),
        [0xFE, 0xFF, rest @ ..] => utf16(rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect()),
        [0xEF, 0xBB, 0xBF, rest @ ..] => text(rest),
        _ => text(bytes),
    }
}

fn text(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        Error::invalid("This file isn't text")
            .with_hint("Open a .sqlplan, an EXPLAIN (FORMAT JSON) file, or EXPLAIN text output.")
    })
}

/// Which plan format the text is in, if Culprit reads it.
pub fn detect(text: &str) -> Option<PlanFormat> {
    let text = clean(text);
    detect_clean(text.trim())
}

/// Read a plan from text in any supported format. The plan keeps the text it came from in `raw` so it can be saved again.
pub fn parse_plan(input: &str) -> Result<Plan> {
    if input.len() > MAX_PLAN_BYTES {
        return Err(too_large(input.len()));
    }
    let cleaned = clean(input);
    // Keep the first line's indentation: text plans are read by column, and psql indents every line by one space.
    let block = cleaned.trim_end().trim_start_matches(['\n', '\r']);
    let block = match block.find(|c: char| !c.is_whitespace()) {
        Some(first) => &block[block[..first].rfind('\n').map_or(0, |n| n + 1)..],
        None => "",
    };
    let text = block.trim_start();
    if text.is_empty() {
        return Err(Error::invalid("There is no plan to read"));
    }
    let format = detect_clean(text).ok_or_else(|| unrecognized(text))?;
    let unreadable = |e: Error| {
        Error::invalid(format!("This looks like {} but Culprit couldn't read it: {}", format.label(), e.message))
    };
    let mut plan = match format {
        PlanFormat::ShowplanXml => showplan(text),
        PlanFormat::PostgresJson => postgres_json(text),
        PlanFormat::YugabyteJson => yugabyte_json(text),
        PlanFormat::PostgresText => postgres_text(block),
        PlanFormat::MysqlJson => mysql_json(text),
        PlanFormat::MysqlTree => mysql_tree(text),
        PlanFormat::SqliteText => sqlite_text(text),
        PlanFormat::Culprit => culprit_json(text),
    }
    .map_err(unreadable)?;
    if format != PlanFormat::Culprit {
        plan.raw = Some(block.into());
    }
    culprit_core::insights::annotate(&mut plan);
    culprit_checks::present_plan(&mut plan, None);
    Ok(plan)
}

/// Every plan a saved scan captured, read again from the text it was captured as (`raw`) by this version's parsers,
/// before anything reads the scan: a parser fix (the inner side of a nested loop, a parallel plan's rows, a guess the
/// collector marks) reaches the scans saved before it, so an old report shows the plan a scan taken now would. What
/// the scan added that the text doesn't hold stays as it was: the statement it was planned for and the kind of plan
/// here, the notes a collector puts on a plan (the server's version, where its sample came from) and what it looked up
/// for it (table sizes) in the provider's own `reparse`, which takes from the scan's own catalog what a plan saved
/// without it lacks. The marks a report's preparation adds (changed columns, index kinds) and the plan's insights are
/// worked out again when it is analyzed. A plan with no text, or text this build can't read, stays as saved; so does
/// SQLite's, whose text keeps the steps but not their tree.
pub fn reparse_saved(snapshot: &mut culprit_core::Snapshot) {
    let _ = reparse_saved_traced(snapshot);
}

pub(crate) fn reparse_saved_traced(snapshot: &mut culprit_core::Snapshot) -> Vec<culprit_core::PlanParsingProvenance> {
    use culprit_core::{PlanParsingOutcome, PlanParsingProvenance};
    let database = snapshot.server.database.clone();
    let Some(workload) = snapshot.workload.as_mut() else { return Vec::new() };
    let mut trace = Vec::new();
    let catalog = Catalog { database: &database, tables: &snapshot.tables, indexes: &snapshot.indexes };
    // A pg_stat_statements entry's plan is planned from the text it keeps; a log's statement is the application's.
    let statement_stats = workload.kind() == Some(culprit_core::WorkloadKind::StatementStats);
    for q in workload.queries.iter_mut() {
        if let Some(history) = &mut q.plan_history
            && let Some(saved) = &history.captured_plan
        {
            // Historical cache plans retain their own evidence. Reparse with no current catalog or statement
            // measurements, which cannot describe the database when this cached plan was compiled.
            let empty = Catalog { database: &database, tables: &[], indexes: &[] };
            let engine = saved.engine;
            let mut outcome =
                if saved.raw.is_some() { PlanParsingOutcome::Retained } else { PlanParsingOutcome::NoNativeText };
            if let Some((fresh, native)) = reparse(saved, None, &empty) {
                if native {
                    outcome = PlanParsingOutcome::Reparsed;
                }
                history.captured_plan = Some(fresh);
            }
            trace.push(PlanParsingProvenance { query: q.id.clone(), engine, parser: parser_version(engine), outcome });
        }
        let Some(saved) = q.plan.as_ref() else { continue };
        let entry = statement_stats.then_some(q.id.as_str());
        let raw = saved.raw.is_some();
        let mut outcome = if raw { PlanParsingOutcome::Retained } else { PlanParsingOutcome::NoNativeText };
        let engine = saved.engine;
        if let Some((fresh, parsed_native)) = reparse(saved, entry, &catalog) {
            if parsed_native {
                outcome = PlanParsingOutcome::Reparsed;
                // This is a derived estimate from the old parser, never an independent measurement.
                if let Some(costs) = &mut q.costs {
                    costs.row_bytes = None;
                }
            }
            q.plan = Some(fresh);
        }
        trace.push(PlanParsingProvenance { query: q.id.clone(), engine, parser: parser_version(engine), outcome });
    }
    culprit_core::signals::fill_row_bytes(&mut workload.queries);
    trace
}

fn parser_version(engine: culprit_core::Engine) -> culprit_core::ComponentVersion {
    match engine {
        #[cfg(feature = "mssql")]
        culprit_core::Engine::Mssql => culprit_mssql::component_version(),
        #[cfg(feature = "postgres")]
        culprit_core::Engine::Postgres => culprit_postgres::component_version(),
        #[cfg(feature = "yugabyte")]
        culprit_core::Engine::Yugabyte => culprit_yugabyte::component_version(),
        #[cfg(feature = "mysql")]
        culprit_core::Engine::Mysql => culprit_mysql::component_version(),
        // SQLite's normalized tree is retained rather than reconstructed from its flat native text.
        _ => culprit_core::ComponentVersion::unknown("no native parser used"),
    }
}

/// What a saved scan knows about its database, for its plans read again.
struct Catalog<'a> {
    database: &'a str,
    tables: &'a [culprit_core::TableStat],
    indexes: &'a [culprit_core::IndexStat],
}

/// `saved` read again by its engine's parser; `entry` is the pg_stat_statements entry it was planned for.
fn reparse(saved: &Plan, entry: Option<&str>, catalog: &Catalog) -> Option<(Plan, bool)> {
    let _ = (entry, catalog.database, catalog.tables, catalog.indexes);
    let reparsed: Option<(Plan, bool)> = match saved.engine {
        #[cfg(feature = "yugabyte")]
        culprit_core::Engine::Yugabyte => culprit_yugabyte::plan::reparse(saved).map(|p| (p, true)),
        #[cfg(feature = "mssql")]
        culprit_core::Engine::Mssql => culprit_mssql::plan_xml::reparse(saved).map(|p| (p, true)),
        #[cfg(feature = "postgres")]
        // A plan saved without its JSON still takes the table sizes it lacks from the scan's catalog.
        culprit_core::Engine::Postgres => {
            culprit_postgres::plan_json::reparse(saved, entry, catalog.tables, catalog.indexes)
                .map(|p| (p, true))
                .or_else(|| {
                    let mut plan = saved.clone();
                    culprit_postgres::plan_json::refill_from_catalog(&mut plan, catalog.tables, catalog.indexes);
                    Some((plan, false))
                })
        }
        #[cfg(feature = "mysql")]
        culprit_core::Engine::Mysql => {
            culprit_mysql::plan_json::reparse(saved, Some(catalog.database).filter(|d| !d.is_empty()))
                .map(|p| (p, true))
        }
        _ => None,
    };
    let (mut plan, parsed_native) = reparsed.filter(|(plan, _)| plan.engine == saved.engine)?;
    plan.kind = saved.kind;
    plan.statement = saved.statement.clone();
    plan.insights = None;
    plan.view = None;
    Some((plan, parsed_native))
}

fn detect_clean(text: &str) -> Option<PlanFormat> {
    if text.starts_with('<') {
        return text.contains("ShowPlanXML").then_some(PlanFormat::ShowplanXml);
    }
    if text.starts_with('{') || text.starts_with('[') {
        let value: Value = serde_json::from_str(text).ok()?;
        return json_format(unwrap_json(value)?.0.as_ref()?);
    }
    let first = text.lines().next()?.trim();
    if is_sqlite_text(text) {
        return Some(PlanFormat::SqliteText);
    }
    if first.starts_with("-> ") {
        return Some(PlanFormat::MysqlTree);
    }
    let pg_marks =
        |line: &str| line.contains("(cost=") || line.contains("(actual ") || line.contains("(never executed)");
    if pg_marks(first) || text.lines().any(|l| l.trim_start().starts_with("->  ")) {
        return Some(PlanFormat::PostgresText);
    }
    None
}

/// The plan object inside tool-specific wrappers: `[{"QUERY PLAN": [...]}]` (pgAdmin, psql -A), `{"EXPLAIN": "…"}`.
/// Returns the value and the statement text when a wrapper carried it.
fn unwrap_json(value: Value) -> Option<(Option<Value>, Option<String>)> {
    let mut value = value;
    let mut statement = None;
    for _ in 0..3 {
        let inner = match &value {
            Value::Array(items) if items.len() == 1 => items[0].as_object().and_then(wrapper_value),
            Value::Object(o) => wrapper_value(o),
            _ => None,
        };
        let Some(inner) = inner else { break };
        value = match inner {
            Value::String(s) => serde_json::from_str(&s).ok()?,
            other => other,
        };
    }
    let top = match &value {
        Value::Array(items) => items.first().and_then(Value::as_object),
        Value::Object(o) => Some(o),
        _ => None,
    };
    if let Some(top) = top {
        statement =
            ["Query Text", "query"].iter().find_map(|k| top.get(*k).and_then(Value::as_str)).map(str::to_string);
    }
    Some((Some(value), statement))
}

fn wrapper_value(o: &Map<String, Value>) -> Option<Value> {
    if o.len() != 1 {
        return None;
    }
    ["QUERY PLAN", "EXPLAIN", "explain"].iter().find_map(|k| o.get(*k).cloned())
}

fn json_format(value: &Value) -> Option<PlanFormat> {
    let top = match value {
        Value::Array(items) => items.first()?,
        other => other,
    };
    let o = top.as_object()?;
    if o.contains_key("Plan") {
        Some(
            if ["Storage Read Requests", "Storage Table Read Requests", "Storage Filter", "Storage Index Filter"]
                .iter()
                .any(|key| contains_key(value, key))
            {
                PlanFormat::YugabyteJson
            } else {
                PlanFormat::PostgresJson
            },
        )
    } else if o.contains_key("query_block") || (o.contains_key("operation") && o.contains_key("access_type")) {
        Some(PlanFormat::MysqlJson)
    } else if o.contains_key("root") && o.contains_key("engine") && o.contains_key("total_cost") {
        Some(PlanFormat::Culprit)
    } else {
        None
    }
}

fn unrecognized(text: &str) -> Error {
    let hint = "Paste or open a SQL Server plan (.sqlplan), PostgreSQL EXPLAIN output (text or FORMAT JSON), MySQL \
                EXPLAIN FORMAT=JSON / EXPLAIN ANALYZE output, or SQLite EXPLAIN QUERY PLAN output.";
    let first = text.lines().next().unwrap_or("").trim();
    let message = if text.contains("<explain xmlns=\"http://www.postgresql.org") || first.starts_with("- Plan:") {
        "PostgreSQL plans in XML or YAML aren't supported; use EXPLAIN (FORMAT JSON) or the default text format"
    } else if text.starts_with('<') {
        "This XML isn't a SQL Server execution plan"
    } else if text.contains("|--") {
        "This is a SQL Server text plan (SHOWPLAN_TEXT); Culprit reads the XML plan (SET SHOWPLAN_XML ON or Save Execution Plan As in SSMS)"
    } else if text.starts_with('{') || text.starts_with('[') {
        "This JSON isn't a plan Culprit knows"
    } else {
        "Culprit doesn't recognize this as a query plan"
    };
    Error::invalid(message).with_hint(hint)
}

fn too_large(len: usize) -> Error {
    Error::invalid(format!("This is {} MB, far larger than any query plan", len / (1024 * 1024)))
}

/// Strip what database clients wrap around a plan: psql's `QUERY PLAN` header, dashes, `+` continuations and
/// `(26 rows)` footer; the mysql client's table borders and `*** 1. row ***` / `EXPLAIN:` labels; quoted grid copies.
fn clean(text: &str) -> String {
    let text = text.trim_start_matches('\u{feff}');
    // The sqlite3 shell heads its tree with the `QUERY PLAN` line psql prints above a plan, dropped below: keep it.
    if is_sqlite_text(text) {
        return text.trim_end().to_string();
    }
    let is_border = |t: &str| !t.is_empty() && t.chars().all(|c| matches!(c, '-' | '+'));
    let is_header = |t: &str| matches!(t.trim_matches('|').trim(), "QUERY PLAN" | "EXPLAIN");
    let is_footer = |t: &str| {
        t.strip_prefix('(')
            .and_then(|r| r.strip_suffix(" rows)").or_else(|| r.strip_suffix(" row)")))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    let is_banner = |t: &str| t.starts_with("***") && t.ends_with("***") && t.contains(". row");
    let decorated = text.lines().any(|l| is_header(l.trim())) && text.lines().any(|l| is_border(l.trim()));
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if is_border(t) || is_header(t) || is_footer(t) || is_banner(t) {
            continue;
        }
        let mut line = line.trim_end().to_string();
        if decorated && line.ends_with('+') {
            line = line.trim_end_matches('+').trim_end().to_string();
        }
        if let Some(rest) = line.trim_start().strip_prefix("EXPLAIN: ") {
            line = rest.to_string();
        }
        if let Some(rest) = line.strip_prefix("| ") {
            line = rest.to_string();
        }
        if line.ends_with(" |") || line.ends_with("\t|") {
            line = line[..line.len() - 1].trim_end().to_string();
        }
        lines.push(line);
    }
    let non_empty: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if !non_empty.is_empty()
        && non_empty.iter().all(|l| l.trim().len() >= 2 && l.trim().starts_with('"') && l.trim().ends_with('"'))
    {
        lines = lines
            .iter()
            .map(|l| {
                let t = l.trim();
                if t.len() >= 2 { t[1..t.len() - 1].replace("\"\"", "\"") } else { t.to_string() }
            })
            .collect();
    }
    lines.join("\n")
}

/// Save plan text to the Downloads folder under a cleaned-up name, never overwriting a file. Returns where it went.
/// Only `.sqlplan`, `.json`, `.txt`, `.html` and `.md` names are written; anything else in the name becomes `-`.
pub fn save_to_downloads(name: &str, contents: &str) -> Result<PathBuf> {
    if contents.len() > MAX_PLAN_BYTES * 2 {
        return Err(too_large(contents.len()));
    }
    let dir = dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .filter(|dir| dir.is_dir())
        .ok_or_else(|| Error::not_found("There is no Downloads folder to save into"))?;
    let (stem, extension) = file_name(name);
    for n in 0..1000 {
        let file = if n == 0 { format!("{stem}.{extension}") } else { format!("{stem} ({n}).{extension}") };
        let path = dir.join(file);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(contents.as_bytes())
                    .map_err(|e| Error::internal(format!("Could not save the plan: {e}")))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::internal(format!("Could not save the plan: {e}"))),
        }
    }
    Err(Error::internal("Too many saved plans with this name"))
}

/// A plain file name from whatever the UI suggested: no separators, no leading dots, a known extension.
fn file_name(name: &str) -> (String, &'static str) {
    let lower = name.to_ascii_lowercase();
    let extension = ["sqlplan", "json", "txt", "html", "md"]
        .into_iter()
        .find(|e| lower.ends_with(&format!(".{e}")))
        .unwrap_or("txt");
    let stem = if lower.ends_with(&format!(".{extension}")) { &name[..name.len() - extension.len() - 1] } else { name };
    let stem: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.') { c } else { '-' })
        .take(80)
        .collect();
    let stem = stem.trim_matches(|c: char| c == '.' || c == ' ' || c == '-').to_string();
    // `con`, `nul`, `com1`… are devices on Windows, whatever the extension.
    (if stem.is_empty() { "plan".to_string() } else { crate::store::windows_safe(stem) }, extension)
}

fn showplan(text: &str) -> Result<Plan> {
    #[cfg(feature = "mssql")]
    {
        // Text pasted from a UTF-16 file keeps its declaration; the string is UTF-8 by now.
        let head = text.floor_char_boundary(120);
        let fixed;
        let text = if text[..head].contains("encoding=\"utf-16\"") {
            fixed = text.replacen("encoding=\"utf-16\"", "encoding=\"utf-8\"", 1);
            fixed.as_str()
        } else if text[..head].contains("encoding='utf-16'") {
            fixed = text.replacen("encoding='utf-16'", "encoding='utf-8'", 1);
            fixed.as_str()
        } else {
            text
        };
        culprit_mssql::plan_xml::parse(text)
    }
    #[cfg(not(feature = "mssql"))]
    {
        let _ = text;
        Err(Error::invalid("This build of Culprit has no SQL Server support"))
    }
}

fn postgres_json(text: &str) -> Result<Plan> {
    let value: Value = serde_json::from_str(text).map_err(|e| Error::invalid(e.to_string()))?;
    let (value, statement) = unwrap_json(value).ok_or_else(|| Error::invalid("No plan inside the JSON"))?;
    postgres_value(&value.unwrap_or(Value::Null), statement.as_deref())
}

fn postgres_value(value: &Value, statement: Option<&str>) -> Result<Plan> {
    #[cfg(feature = "postgres")]
    {
        let kind = if contains_key(value, "Actual Loops") { PlanKind::Actual } else { PlanKind::Estimated };
        culprit_postgres::plan_json::from_value(value, kind, statement)
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (value, statement);
        Err(Error::invalid("This build of Culprit has no PostgreSQL support"))
    }
}

fn yugabyte_json(text: &str) -> Result<Plan> {
    #[cfg(feature = "yugabyte")]
    {
        let value: Value = serde_json::from_str(text).map_err(|e| Error::invalid(e.to_string()))?;
        let (value, statement) = unwrap_json(value).ok_or_else(|| Error::invalid("No plan inside the JSON"))?;
        let value = value.unwrap_or(Value::Null);
        let kind = if contains_key(&value, "Actual Loops") { PlanKind::Actual } else { PlanKind::Estimated };
        culprit_yugabyte::plan::from_value(&value, kind, statement.as_deref())
    }
    #[cfg(not(feature = "yugabyte"))]
    {
        let _ = text;
        Err(Error::invalid("This build of Culprit has no YugabyteDB support"))
    }
}

fn postgres_text(text: &str) -> Result<Plan> {
    postgres_value(&pg_text::to_json(text)?, None)
}

fn mysql_json(text: &str) -> Result<Plan> {
    let value: Value = serde_json::from_str(text).map_err(|e| Error::invalid(e.to_string()))?;
    let (value, statement) = unwrap_json(value).ok_or_else(|| Error::invalid("No plan inside the JSON"))?;
    let value = value.unwrap_or(Value::Null);
    let value = match value {
        Value::Array(mut items) if !items.is_empty() => items.swap_remove(0),
        other => other,
    };
    #[cfg(feature = "mysql")]
    {
        if value.get("query_block").is_some() {
            let json = serde_json::to_string(&value).map_err(|e| Error::internal(e.to_string()))?;
            return culprit_mysql::plan_json::parse(&json, PlanKind::Estimated, statement.as_deref(), None);
        }
        // Version 2 nodes carry the same operation text as the tree format; draw them through the tree reader.
        let mut tree = String::new();
        mysql_v2::tree(&value, 0, &mut tree);
        let mut plan = culprit_mysql::plan_tree::parse(&tree, statement.as_deref(), None)?;
        plan.kind = if contains_key(&value, "actual_rows") { PlanKind::Actual } else { PlanKind::Estimated };
        mysql_v2::fill_objects(&value, &mut plan.root);
        Ok(plan)
    }
    #[cfg(not(feature = "mysql"))]
    {
        let _ = (value, statement);
        Err(Error::invalid("This build of Culprit has no MySQL support"))
    }
}

fn is_sqlite_text(text: &str) -> bool {
    #[cfg(feature = "sqlite")]
    return culprit_sqlite::plan::is_text_plan(text);
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = text;
        false
    }
}

fn sqlite_text(text: &str) -> Result<Plan> {
    #[cfg(feature = "sqlite")]
    return culprit_sqlite::plan::parse_text(text);
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = text;
        Err(Error::invalid("This build of Culprit has no SQLite support"))
    }
}

fn mysql_tree(text: &str) -> Result<Plan> {
    #[cfg(feature = "mysql")]
    {
        let mut plan = culprit_mysql::plan_tree::parse(text, None, None)?;
        if !text.contains("(actual ") && !text.contains("(never executed)") {
            plan.kind = PlanKind::Estimated;
        }
        Ok(plan)
    }
    #[cfg(not(feature = "mysql"))]
    {
        let _ = text;
        Err(Error::invalid("This build of Culprit has no MySQL support"))
    }
}

/// A plan Culprit exported earlier. Node ids are renumbered so a hand-edited file can't confuse the UI.
fn culprit_json(text: &str) -> Result<Plan> {
    let mut plan: Plan = serde_json::from_str(text).map_err(|e| Error::invalid(e.to_string()))?;
    fn renumber(node: &mut PlanNode, next: &mut u32) {
        node.id = *next;
        *next += 1;
        node.children.iter_mut().for_each(|c| renumber(c, next));
    }
    renumber(&mut plan.root, &mut 0);
    Ok(plan)
}

#[allow(dead_code)]
fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(o) => o.contains_key(key) || o.values().any(|v| contains_key(v, key)),
        Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

#[cfg(feature = "mysql")]
mod mysql_v2 {
    use culprit_core::PlanNode;
    use serde_json::Value;

    /// Render a version 2 node as an EXPLAIN tree line, then its inputs four spaces deeper.
    pub fn tree(node: &Value, depth: usize, out: &mut String) {
        let op = node.get("operation").and_then(Value::as_str).unwrap_or("Operation");
        out.push_str(&"    ".repeat(depth));
        out.push_str("-> ");
        out.push_str(op.lines().next().unwrap_or(op));
        let num = |k: &str| node.get(k).and_then(Value::as_f64);
        let mut stats = Vec::new();
        if let Some(cost) = num("estimated_total_cost") {
            stats.push(format!("cost={cost}"));
        }
        if let Some(rows) = num("estimated_rows") {
            stats.push(format!("rows={rows}"));
        }
        if !stats.is_empty() {
            out.push_str(&format!("  ({})", stats.join(" ")));
        }
        if let Some(rows) = num("actual_rows") {
            let first = num("actual_first_row_ms").unwrap_or(0.0);
            let last = num("actual_last_row_ms").unwrap_or(first);
            let loops = num("actual_loops").unwrap_or(1.0);
            out.push_str(&format!(" (actual time={first}..{last} rows={rows} loops={loops})"));
        }
        out.push('\n');
        for input in node.get("inputs").and_then(Value::as_array).into_iter().flatten() {
            tree(input, depth + 1, out);
        }
    }

    /// Version 2 names each table and its schema, which the tree text only gives as an alias. Walks both trees
    /// together and stops wherever their shapes differ.
    pub fn fill_objects(json: &Value, node: &mut PlanNode) {
        if let Some(table) = json.get("table_name").and_then(Value::as_str) {
            node.object = Some(match json.get("schema_name").and_then(Value::as_str) {
                Some(schema) => format!("{schema}.{table}"),
                None => table.to_string(),
            });
        }
        if let Some(index) = json.get("index_name").and_then(Value::as_str) {
            node.index = Some(index.to_string());
        }
        let inputs: &[Value] = json.get("inputs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        if inputs.len() == node.children.len() {
            for (input, child) in inputs.iter().zip(node.children.iter_mut()) {
                fill_objects(input, child);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::file_name;

    #[test]
    fn saved_file_names_stay_inside_the_folder() {
        assert_eq!(file_name("Plan 815.sqlplan"), ("Plan 815".into(), "sqlplan"));
        assert_eq!(file_name("../../etc/passwd"), ("etc-passwd".into(), "txt"));
        assert_eq!(file_name("..\\..\\boot.ini.json"), ("boot.ini".into(), "json"));
        assert_eq!(file_name(".hidden.txt"), ("hidden".into(), "txt"));
        assert_eq!(file_name("/abs/path/x.exe"), ("abs-path-x.exe".into(), "txt"));
        assert_eq!(file_name("..."), ("plan".into(), "txt"));
        assert_eq!(file_name("con"), ("_con".into(), "txt"));
        assert_eq!(file_name("COM1.sqlplan"), ("_COM1".into(), "sqlplan"));
        assert_eq!(file_name("nul.json"), ("_nul".into(), "json"));
    }
}
