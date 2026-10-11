//! Plans from outside a connection: pasted text or files saved by other tools. Reads SQL Server showplan XML
//! (.sqlplan), Postgres EXPLAIN as JSON or text, MySQL EXPLAIN FORMAT=JSON (versions 1 and 2) and MySQL's tree output
//! (EXPLAIN ANALYZE, FORMAT=TREE), plus Culprit's own plan JSON. psql and mysql client decorations are stripped first.

use std::io::Write;
use std::path::PathBuf;

use culprit_core::{Error, Plan, PlanKind, PlanNode, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use ts_rs::TS;

/// The largest SQL Server plans run to a few megabytes; anything far beyond that is not a plan.
pub const MAX_PLAN_BYTES: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanFormat {
    ShowplanXml,
    PostgresJson,
    PostgresText,
    MysqlJson,
    MysqlTree,
    Culprit,
}

impl PlanFormat {
    pub fn label(self) -> &'static str {
        match self {
            PlanFormat::ShowplanXml => "SQL Server showplan XML",
            PlanFormat::PostgresJson => "Postgres EXPLAIN (FORMAT JSON)",
            PlanFormat::PostgresText => "Postgres EXPLAIN",
            PlanFormat::MysqlJson => "MySQL EXPLAIN FORMAT=JSON",
            PlanFormat::MysqlTree => "MySQL EXPLAIN ANALYZE / FORMAT=TREE",
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
        PlanFormat::PostgresText => postgres_text(block),
        PlanFormat::MysqlJson => mysql_json(text),
        PlanFormat::MysqlTree => mysql_tree(text),
        PlanFormat::Culprit => culprit_json(text),
    }
    .map_err(unreadable)?;
    if format != PlanFormat::Culprit {
        plan.raw = Some(block.to_string());
    }
    culprit_core::insights::annotate(&mut plan);
    Ok(plan)
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
        Some(PlanFormat::PostgresJson)
    } else if o.contains_key("query_block") || (o.contains_key("operation") && o.contains_key("access_type")) {
        Some(PlanFormat::MysqlJson)
    } else if o.contains_key("root") && o.contains_key("engine") && o.contains_key("total_cost") {
        Some(PlanFormat::Culprit)
    } else {
        None
    }
}

fn unrecognized(text: &str) -> Error {
    let hint = "Paste or open a SQL Server plan (.sqlplan), Postgres EXPLAIN output (text or FORMAT JSON), or MySQL \
                EXPLAIN FORMAT=JSON / EXPLAIN ANALYZE output.";
    let first = text.lines().next().unwrap_or("").trim();
    let message = if text.contains("<explain xmlns=\"http://www.postgresql.org") || first.starts_with("- Plan:") {
        "Postgres plans in XML or YAML aren't supported; use EXPLAIN (FORMAT JSON) or the default text format"
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
    (if stem.is_empty() { "plan".to_string() } else { stem }, extension)
}

fn showplan(text: &str) -> Result<Plan> {
    #[cfg(feature = "mssql")]
    {
        // Text pasted from a UTF-16 file keeps its declaration; the string is UTF-8 by now.
        let head = text.len().min(120);
        let fixed;
        let text = if text[..head].contains("encoding=\"utf-16\"") {
            fixed = text.replacen("encoding=\"utf-16\"", "encoding=\"utf-8\"", 1);
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
        Err(Error::invalid("This build of Culprit has no Postgres support"))
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

/// Postgres's text EXPLAIN, rebuilt as the JSON EXPLAIN produces so one reader handles both.
mod pg_text {
    use culprit_core::{Error, Result};
    use serde_json::{Map, Value, json};

    struct Frame {
        /// Column where this node's header (or the marker text) starts; its details sit to the right of it.
        col: usize,
        node: Option<Map<String, Value>>,
        marker: Option<String>,
        children: Vec<Value>,
    }

    pub fn to_json(text: &str) -> Result<Value> {
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let base = lines.first().map(|l| indent(l)).unwrap_or(0);
        let mut stack: Vec<Frame> = Vec::new();
        let mut root: Option<Value> = None;
        let mut top = Map::new();
        let mut section: Option<String> = None;

        for (n, line) in lines.iter().enumerate() {
            let col = indent(line).saturating_sub(base);
            let body = line.trim();
            if n == 0 {
                let header = body.strip_prefix("->").map(str::trim_start).unwrap_or(body);
                stack.push(Frame { col, node: Some(node_header(header)), marker: None, children: Vec::new() });
                continue;
            }
            if root.is_some() || col == 0 {
                // After the tree: "Planning Time: 0.2 ms", "Execution Time", "Planning:" / "JIT:" blocks.
                close(&mut stack, &mut root, |_| false);
                let (key, value) = body.split_once(':').map(|(k, v)| (k.trim(), v.trim())).unwrap_or((body, ""));
                if col == 0 {
                    section = value.is_empty().then(|| key.to_string());
                    if !value.is_empty() {
                        top.insert(key.to_string(), scalar(key, value));
                    }
                } else if let Some(section) = &section {
                    top.insert(format!("{section} {key}"), scalar(key, value));
                }
                continue;
            }
            if let Some(rest) = body.strip_prefix("->") {
                let header = rest.trim_start();
                close(&mut stack, &mut root, |f| f.col < col);
                let mut node = node_header(header);
                if let Some(name) = stack.last().and_then(|f| f.marker.clone()) {
                    let relationship = name.split_whitespace().next().unwrap_or("SubPlan").to_string();
                    node.insert("Parent Relationship".into(), Value::String(relationship));
                    node.insert("Subplan Name".into(), Value::String(name));
                }
                let header_col = col + (body.len() - header.len());
                stack.push(Frame { col: header_col, node: Some(node), marker: None, children: Vec::new() });
                continue;
            }
            close(&mut stack, &mut root, |f| f.col < col);
            if is_marker(body) {
                stack.push(Frame { col, node: None, marker: Some(body.to_string()), children: Vec::new() });
                continue;
            }
            if let Some(node) = stack.iter_mut().rev().find_map(|f| f.node.as_mut()) {
                details(node, body);
            }
        }
        close(&mut stack, &mut root, |_| false);
        let plan = root.ok_or_else(|| Error::invalid("No plan nodes found"))?;
        let mut out = Map::new();
        out.insert("Plan".into(), plan);
        out.extend(top);
        Ok(Value::Array(vec![Value::Object(out)]))
    }

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    fn is_marker(body: &str) -> bool {
        let first = body.split_whitespace().next().unwrap_or("");
        matches!(first, "InitPlan" | "SubPlan" | "CTE") && !body.contains(": ")
    }

    /// Pop finished frames, attaching each node to the node below it (markers only label the next child).
    fn close(stack: &mut Vec<Frame>, root: &mut Option<Value>, keep: impl Fn(&Frame) -> bool) {
        while let Some(frame) = stack.last() {
            if keep(frame) {
                break;
            }
            let frame = stack.pop().expect("checked above");
            let Some(mut node) = frame.node else { continue };
            if !frame.children.is_empty() {
                node.insert("Plans".into(), Value::Array(frame.children));
            }
            match stack.iter_mut().rev().find(|f| f.node.is_some()) {
                Some(parent) => parent.children.push(Value::Object(node)),
                None => *root = Some(Value::Object(node)),
            }
        }
    }

    /// `Index Scan Backward using ix on public.orders o  (cost=0.43..8.45 rows=1 width=8) (actual time=… rows=1 loops=1)`.
    fn node_header(header: &str) -> Map<String, Value> {
        let mut map = Map::new();
        let cut = ["(cost=", "(actual ", "(never executed)"]
            .iter()
            .filter_map(|m| header.find(m))
            .min()
            .unwrap_or(header.len());
        let (name, stats) = header.split_at(cut);
        if let Some(cost) = group(stats, "(cost=") {
            let mut parts = cost.split_whitespace();
            if let Some((startup, total)) = parts.next().and_then(|r| r.split_once("..")) {
                insert_num(&mut map, "Startup Cost", startup);
                insert_num(&mut map, "Total Cost", total);
            }
            for part in parts {
                match part.split_once('=') {
                    Some(("rows", v)) => insert_num(&mut map, "Plan Rows", v),
                    Some(("width", v)) => insert_num(&mut map, "Plan Width", v),
                    _ => {}
                }
            }
        }
        if let Some(actual) = group(stats, "(actual ") {
            for part in actual.split_whitespace() {
                match part.split_once('=') {
                    Some(("time", v)) => {
                        if let Some((a, b)) = v.split_once("..") {
                            insert_num(&mut map, "Actual Startup Time", a);
                            insert_num(&mut map, "Actual Total Time", b);
                        }
                    }
                    Some(("rows", v)) => insert_num(&mut map, "Actual Rows", v),
                    Some(("loops", v)) => insert_num(&mut map, "Actual Loops", v),
                    _ => {}
                }
            }
        } else if stats.contains("(never executed)") {
            map.insert("Actual Rows".into(), json!(0));
            map.insert("Actual Loops".into(), json!(0));
            map.insert("Actual Total Time".into(), json!(0));
        }
        node_name(name.trim(), &mut map);
        map
    }

    fn group<'a>(stats: &'a str, open: &str) -> Option<&'a str> {
        let start = stats.find(open)? + open.len();
        let end = stats[start..].find(')')? + start;
        Some(&stats[start..end])
    }

    fn node_name(name: &str, map: &mut Map<String, Value>) {
        let mut name = name.to_string();
        if let Some(rest) = name.strip_prefix("Parallel ") {
            map.insert("Parallel Aware".into(), json!(true));
            name = rest.to_string();
        }
        if let Some(rest) = name.strip_prefix("Async ") {
            map.insert("Async Capable".into(), json!(true));
            name = rest.to_string();
        }
        for (prefix, mode) in [("Partial ", "Partial"), ("Finalize ", "Finalize")] {
            if let Some(rest) = name.strip_prefix(prefix) {
                map.insert("Partial Mode".into(), json!(mode));
                name = rest.to_string();
            }
        }
        for op in ["Insert", "Update", "Delete", "Merge"] {
            if let Some(target) = name.strip_prefix(&format!("{op} on ")) {
                map.insert("Node Type".into(), json!("ModifyTable"));
                map.insert("Operation".into(), json!(op));
                relation(target, map);
                return;
            }
        }
        let (head, index, target) = match name.split_once(" using ") {
            Some((head, rest)) => match rest.split_once(" on ") {
                Some((index, target)) => (head.to_string(), Some(index.to_string()), Some(target.to_string())),
                None => (head.to_string(), Some(rest.to_string()), None),
            },
            None => match name.split_once(" on ") {
                Some((head, target)) => (head.to_string(), None, Some(target.to_string())),
                None => (name.clone(), None, None),
            },
        };
        let mut node_type = head.clone();
        if let Some(scan) = head.strip_suffix(" Backward") {
            node_type = scan.to_string();
            map.insert("Scan Direction".into(), json!("Backward"));
        }
        if let Some(index) = index {
            map.insert("Index Name".into(), json!(unquote(&index)));
        }
        if node_type.starts_with("Custom Scan") {
            node_type = "Custom Scan".into();
        }
        // "Hash Left Join" → Hash Join + Left; "Nested Loop Anti Join" → Nested Loop + Anti.
        for join in ["Nested Loop", "Hash", "Merge"] {
            if let Some(rest) = node_type.strip_prefix(join).map(str::trim)
                && (rest == "Join" || rest.ends_with(" Join") || (join == "Nested Loop" && rest.is_empty()))
            {
                let kind =
                    rest.strip_suffix("Join").map(str::trim).filter(|k| !k.is_empty()).unwrap_or("Inner").to_string();
                map.insert("Join Type".into(), json!(kind));
                node_type = if join == "Nested Loop" { join.to_string() } else { format!("{join} Join") };
                break;
            }
        }
        let aggregate = match node_type.as_str() {
            "Aggregate" => Some("Plain"),
            "HashAggregate" => Some("Hashed"),
            "GroupAggregate" => Some("Sorted"),
            "MixedAggregate" => Some("Mixed"),
            _ => None,
        };
        if let Some(strategy) = aggregate {
            node_type = "Aggregate".into();
            map.insert("Strategy".into(), json!(strategy));
        }
        for (prefix, strategy) in [("HashSetOp ", "Hashed"), ("SetOp ", "Sorted")] {
            if let Some(command) = node_type.strip_prefix(prefix).map(str::to_string) {
                node_type = "SetOp".into();
                map.insert("Strategy".into(), json!(strategy));
                map.insert("Command".into(), json!(command));
            }
        }
        if let Some(target) = target {
            match node_type.as_str() {
                "Bitmap Index Scan" => {
                    map.insert("Index Name".into(), json!(unquote(target.trim())));
                }
                "Function Scan" | "Table Function Scan" => {
                    let (name, alias) = tokens(&target);
                    map.insert("Function Name".into(), json!(name));
                    if let Some(alias) = alias {
                        map.insert("Alias".into(), json!(alias));
                    }
                }
                "CTE Scan" | "WorkTable Scan" => {
                    let (name, alias) = tokens(&target);
                    map.insert("CTE Name".into(), json!(name));
                    map.insert("Alias".into(), json!(alias.unwrap_or(name)));
                }
                "Subquery Scan" | "Values Scan" | "Named Tuplestore Scan" => {
                    map.insert("Alias".into(), json!(tokens(&target).0));
                }
                _ => relation(&target, map),
            }
        }
        map.insert("Node Type".into(), json!(node_type));
    }

    /// `public.orders o` → Schema, Relation Name, Alias.
    fn relation(target: &str, map: &mut Map<String, Value>) {
        let (qualified, alias) = tokens(target);
        let (schema, table) = split_qualified(&qualified);
        if let Some(schema) = schema {
            map.insert("Schema".into(), json!(schema));
        }
        map.insert("Relation Name".into(), json!(table));
        map.insert("Alias".into(), json!(alias.unwrap_or_else(|| table.clone())));
    }

    /// First and second whitespace-separated tokens, honoring double quotes.
    fn tokens(text: &str) -> (String, Option<String>) {
        let mut out = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for c in text.trim().chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    current.push(c);
                }
                c if c.is_whitespace() && !quoted => {
                    if !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        let mut it = out.into_iter();
        let first = it.next().unwrap_or_default();
        (unquote_parts(&first), it.next().map(|a| unquote(&a)))
    }

    fn split_qualified(name: &str) -> (Option<String>, String) {
        match name.rsplit_once('.') {
            Some((schema, table)) if !schema.is_empty() => (Some(schema.to_string()), table.to_string()),
            _ => (None, name.to_string()),
        }
    }

    fn unquote(s: &str) -> String {
        let s = s.trim();
        if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
            s[1..s.len() - 1].replace("\"\"", "\"")
        } else {
            s.to_string()
        }
    }

    fn unquote_parts(s: &str) -> String {
        s.split('.').map(unquote).collect::<Vec<_>>().join(".")
    }

    /// `Sort Method: quicksort  Memory: 25kB`, `Buckets: 1024  Batches: 1  Memory Usage: 9kB`, `Filter: (…)`.
    fn details(node: &mut Map<String, Value>, body: &str) {
        let segments: Vec<&str> = body.split("  ").map(str::trim).filter(|s| !s.is_empty()).collect();
        let pairs: Vec<(&str, &str)> = if segments.len() > 1 && segments.iter().all(|s| s.contains(": ")) {
            segments.iter().filter_map(|s| s.split_once(": ")).collect()
        } else {
            match body.split_once(": ").or_else(|| body.strip_suffix(':').map(|k| (k, ""))) {
                Some(pair) => vec![pair],
                None => return,
            }
        };
        let hash = node.get("Node Type").and_then(Value::as_str) == Some("Hash");
        let mut previous = "";
        for (key, value) in pairs {
            let key = key.trim();
            let value = value.trim();
            match key {
                "Memory" | "Disk" if previous == "Sort Method" => {
                    node.insert("Sort Space Type".into(), json!(key));
                    insert_num(node, "Sort Space Used", value.trim_end_matches("kB"));
                }
                "Batches" => {
                    let (now, originally) = match value.split_once(" (originally ") {
                        Some((now, orig)) => (now, Some(orig.trim_end_matches(')'))),
                        None => (value, None),
                    };
                    let name = if hash { "Hash Batches" } else { "HashAgg Batches" };
                    insert_num(node, name, now);
                    if let Some(orig) = originally {
                        insert_num(node, "Original Hash Batches", orig);
                    }
                }
                "Buckets" => {
                    insert_num(node, "Hash Buckets", value.split_whitespace().next().unwrap_or(value));
                }
                _ => {
                    node.insert(key.to_string(), scalar(key, value));
                }
            }
            previous = key;
        }
    }

    fn scalar(key: &str, value: &str) -> Value {
        let number = if key.ends_with("Time") { value.trim_end_matches(" ms") } else { value };
        match number.parse::<f64>() {
            Ok(n) if n.is_finite() => json!(n),
            _ => Value::String(value.to_string()),
        }
    }

    fn insert_num(map: &mut Map<String, Value>, key: &str, value: &str) {
        if let Ok(n) = value.trim().parse::<f64>()
            && n.is_finite()
        {
            map.insert(key.into(), json!(n));
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
    }
}
