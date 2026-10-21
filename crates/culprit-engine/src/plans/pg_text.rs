//! Postgres's text EXPLAIN, rebuilt as the JSON EXPLAIN produces so one reader handles both.

use culprit_core::{Error, Result};
use serde_json::{Map, Value, json};

struct Frame {
    /// Column where this node's header (or the marker text) starts; its details sit to the right of it.
    col: usize,
    node: Option<Map<String, Value>>,
    marker: Option<String>,
    children: Vec<Value>,
}

pub(super) fn to_json(text: &str) -> Result<Value> {
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
    let cut =
        ["(cost=", "(actual ", "(never executed)"].iter().filter_map(|m| header.find(m)).min().unwrap_or(header.len());
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
