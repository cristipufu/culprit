use culprit_core::{
    Engine, Error, NodeKind, NodeRuntime, Plan, PlanIndexHint, PlanKind, PlanNode, PlanWarning, Properties, Result,
};
use roxmltree::{Document, Node, ParsingOptions};

/// RelOp children that describe the operator rather than being the operator itself.
const RELOP_METADATA: &[&str] =
    &["OutputList", "Warnings", "MemoryFractions", "RunTimeInformation", "RunTimePartitionSummary", "InternalInfo"];

/// RelOp attributes worth showing in the node detail panel.
const RELOP_PROPS: &[&str] = &[
    "PhysicalOp",
    "LogicalOp",
    "EstimateRows",
    "EstimatedRowsRead",
    "TableCardinality",
    "EstimateIO",
    "EstimateCPU",
    "AvgRowSize",
    "EstimatedTotalSubtreeCost",
    "EstimateRebinds",
    "EstimateRewinds",
    "EstimatedExecutionMode",
    "Parallel",
    "Partitioned",
];

const STATEMENT_PROPS: &[&str] = &[
    "StatementOptmLevel",
    "StatementOptmEarlyAbortReason",
    "CardinalityEstimationModelVersion",
    "QueryHash",
    "QueryPlanHash",
    "StatementEstRows",
    "StatementSubTreeCost",
];

const QUERY_PLAN_PROPS: &[&str] =
    &["DegreeOfParallelism", "NonParallelPlanReason", "CachedPlanSize", "CompileTime", "CompileCPU", "CompileMemory"];

/// Rows read must exceed rows returned by this factor before a residual predicate is worth flagging.
const RESIDUAL_READ_FACTOR: f64 = 10.0;
const RESIDUAL_MIN_ROWS_READ: f64 = 1_000.0;
const MAX_LIST_PROP_LEN: usize = 600;

/// Every statement in a showplan document, by the type SQL Server gives it (`SELECT`, `COMMIT TRANSACTION`, `COND`…),
/// nested ones included (the branches of an IF are statements of their own).
pub fn statement_types(xml: &str) -> Result<Vec<String>> {
    let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
    let doc = Document::parse_with_options(xml, options)
        .map_err(|e| Error::internal(format!("Could not read the SQL Server plan XML: {e}")))?;
    Ok(doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name().starts_with("Stmt"))
        .filter_map(|n| n.attribute("StatementType").map(str::to_string))
        .collect())
}

pub fn parse(xml: &str) -> Result<Plan> {
    parse_statement(xml, None)
}

/// Parse the statement whose `QueryHash` matches (a batch or procedure plan holds every statement), or the most
/// expensive statement when none matches.
pub fn parse_statement(xml: &str, query_hash: Option<&str>) -> Result<Plan> {
    let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
    let doc = Document::parse_with_options(xml, options)
        .map_err(|e| Error::internal(format!("Could not read the SQL Server plan XML: {e}")))?;
    let candidates =
        || doc.descendants().filter(|n| n.has_tag_name_local("StmtSimple") && child(*n, "QueryPlan").is_some());
    let matching = query_hash.and_then(|hash| {
        candidates().find(|n| n.attribute("QueryHash").is_some_and(|h| h.eq_ignore_ascii_case(hash.trim())))
    });
    let statement = matching
        .or_else(|| {
            candidates()
                .max_by(|a, b| attr_f64(*a, "StatementSubTreeCost").total_cmp(&attr_f64(*b, "StatementSubTreeCost")))
        })
        .ok_or_else(|| Error::internal("The plan XML contains no statement with a query plan"))?;
    let query_plan = child(statement, "QueryPlan").expect("filtered on QueryPlan above");
    let root_op = child(query_plan, "RelOp").ok_or_else(|| Error::internal("The query plan has no operators"))?;

    let mut root = parse_relop(root_op);
    let total_cost =
        statement.attribute("StatementSubTreeCost").and_then(|v| v.parse().ok()).unwrap_or(root.subtree_cost);
    let is_actual = doc.descendants().any(|n| n.has_tag_name_local("RunTimeInformation"));

    let mut props = Properties::new();
    copy_attrs(statement, STATEMENT_PROPS, &mut props);
    copy_attrs(query_plan, QUERY_PLAN_PROPS, &mut props);
    if let Some(grant) = child(query_plan, "MemoryGrantInfo") {
        for (attr, key) in [
            ("SerialDesiredMemory", "MemoryDesiredKB"),
            ("RequestedMemory", "MemoryRequestedKB"),
            ("GrantedMemory", "MemoryGrantedKB"),
            ("MaxUsedMemory", "MemoryMaxUsedKB"),
            ("GrantWaitTime", "MemoryGrantWaitMs"),
        ] {
            if let Some(v) = grant.attribute(attr) {
                props.insert(key.into(), v.into());
            }
        }
    }
    if let Some(time) = child(query_plan, "QueryTimeStats") {
        for (attr, key) in [("ElapsedTime", "ElapsedMs"), ("CpuTime", "CpuMs")] {
            if let Some(v) = time.attribute(attr) {
                props.insert(key.into(), v.into());
            }
        }
    }
    if let Some(threads) = child(query_plan, "ThreadStat").and_then(|t| t.attribute("UsedThreads")) {
        props.insert("UsedThreads".into(), threads.into());
    }
    if let Some(params) = child(query_plan, "ParameterList") {
        let list: Vec<String> = children(params, "ColumnReference")
            .map(|p| {
                let name = p.attribute("Column").unwrap_or("?");
                match p.attribute("ParameterCompiledValue") {
                    Some(v) => format!("{name} = {v}"),
                    None => name.to_string(),
                }
            })
            .collect();
        if !list.is_empty() {
            props.insert("Parameters".into(), truncate(list.join(", ")));
        }
    }

    let warnings = child(query_plan, "Warnings").map(warnings_of).unwrap_or_default();
    if let Some(converts) = child(query_plan, "Warnings").and_then(convert_issues) {
        props.insert("PlanAffectingConvert".into(), converts);
    }
    if let Some(kind) = child(query_plan, "Warnings")
        .and_then(|w| child(w, "MemoryGrantWarning"))
        .and_then(|g| g.attribute("GrantWarningKind"))
    {
        props.insert("MemoryGrantWarning".into(), kind.into());
    }
    if is_actual {
        flag_misestimates(&mut root);
    }

    let mut plan = Plan {
        engine: Engine::Mssql,
        kind: if is_actual { PlanKind::Actual } else { PlanKind::Estimated },
        statement: statement.attribute("StatementText").map(|s| s.trim().to_string()),
        total_cost,
        root,
        warnings,
        index_hints: index_hints(query_plan),
        props,
        raw: Some(xml.to_string()),
        insights: None,
    };
    plan.compute_self_costs();
    Ok(plan)
}

fn parse_relop(relop: Node) -> PlanNode {
    let physical = relop.attribute("PhysicalOp").unwrap_or("Unknown").to_string();
    let logical = relop.attribute("LogicalOp").unwrap_or_default().to_string();
    let operator = operator_element(relop);
    let is_lookup =
        operator.and_then(|o| o.attribute("Lookup")).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let op =
        if is_lookup && physical.contains("Clustered Index") { "Key Lookup".to_string() } else { physical.clone() };

    let mut props = Properties::new();
    copy_attrs(relop, RELOP_PROPS, &mut props);
    if let Some(o) = operator {
        for attr in [
            "Ordered",
            "ScanDirection",
            "ForcedIndex",
            "ForceSeek",
            "ForceScan",
            "NoExpandHint",
            "Storage",
            "ManyToMany",
        ] {
            if let Some(v) = o.attribute(attr) {
                props.insert(attr.into(), v.into());
            }
        }
    }
    if let Some(output) = child(relop, "OutputList") {
        let columns: Vec<&str> = children(output, "ColumnReference").filter_map(|c| c.attribute("Column")).collect();
        if !columns.is_empty() {
            props.insert("OutputList".into(), truncate(columns.join(", ")));
        }
    }

    // Scalar functions this operator calls (not those of operators below it): each one runs once per row.
    let functions: Vec<String> = relop
        .descendants()
        .filter(|n| n.has_tag_name_local("UserDefinedFunction"))
        .filter(|n| n.ancestors().skip(1).find(|a| a.has_tag_name_local("RelOp")) == Some(relop))
        .filter_map(|n| n.attribute("FunctionName"))
        .map(|name| {
            // [db].[schema].[function] → schema.function
            let parts: Vec<String> = name.split("].[").map(unbracket).collect();
            parts[parts.len().saturating_sub(2)..].join(".")
        })
        .collect();
    if !functions.is_empty() {
        props.insert("UserDefinedFunctions".into(), truncate(functions.join(", ")));
    }
    if let Some(w) = child(relop, "Warnings") {
        // Pages a spill wrote to tempdb (8 KB each), summed over sort, hash and exchange spills.
        let pages: f64 = w
            .children()
            .filter(|d| matches!(d.tag_name().name(), "SortSpillDetails" | "HashSpillDetails" | "ExchangeSpillDetails"))
            .map(|d| attr_f64(d, "WritesToTempDb"))
            .sum();
        if pages > 0.0 {
            props.insert("TempdbWritesPages".into(), pages.to_string());
        }
    }
    if let Some(columns) = child(relop, "Warnings").and_then(|w| child(w, "ColumnsWithNoStatistics")) {
        let names: Vec<String> = children(columns, "ColumnReference").map(column_name).map(|c| unbracket(&c)).collect();
        if !names.is_empty() {
            props.insert("ColumnsWithNoStatistics".into(), truncate(names.join(", ")));
        }
    }

    let (object, index) = operator.and_then(object_of).unwrap_or((None, None));
    let seek_predicate = operator.and_then(seek_predicate_of);
    let predicate = operator.and_then(predicate_of);

    let est_rows = attr_f64(relop, "EstimateRows");
    let est_executions = 1.0 + attr_f64(relop, "EstimateRebinds") + attr_f64(relop, "EstimateRewinds");
    let rows_read = relop.attribute("EstimatedRowsRead").and_then(|v| v.parse().ok());
    let table_rows = relop.attribute("TableCardinality").and_then(|v| v.parse().ok());

    let (act_rows, act_executions, act_time_ms, mut runtime) = match child(relop, "RunTimeInformation") {
        Some(rt) => runtime_counters(rt, &mut props),
        None => (None, None, None, None),
    };
    // Sort/hash/exchange spill details say how many pages went to tempdb.
    let spilled: f64 = child(relop, "Warnings")
        .map(|w| {
            w.descendants().filter_map(|d| d.attribute("WritesToTempDb")).filter_map(|v| v.parse::<f64>().ok()).sum()
        })
        .unwrap_or(0.0);
    if spilled > 0.0 {
        runtime.get_or_insert_with(NodeRuntime::default).spill_pages = Some(spilled);
    }

    let mut warnings = child(relop, "Warnings").map(warnings_of).unwrap_or_default();
    let read_compared = props.get("ActualRowsRead").and_then(|v| v.parse::<f64>().ok()).or(rows_read);
    let returned = act_rows.unwrap_or(est_rows * est_executions);
    if predicate.is_some()
        && read_compared
            .is_some_and(|read| read >= RESIDUAL_MIN_ROWS_READ && read >= RESIDUAL_READ_FACTOR * returned.max(1.0))
    {
        push_unique(&mut warnings, PlanWarning::ResidualPredicate);
    }

    let mut child_ops = Vec::new();
    if let Some(o) = operator {
        collect_relops(o, &mut child_ops);
    }

    PlanNode {
        id: relop.attribute("NodeId").and_then(|v| v.parse().ok()).unwrap_or(0),
        kind: node_kind(&op, &logical),
        detail: (!logical.is_empty() && logical != physical && logical != op).then_some(logical),
        op,
        object,
        index,
        seek_predicate,
        predicate,
        est_rows,
        est_executions,
        act_rows,
        act_executions,
        rows_read,
        table_rows,
        subtree_cost: attr_f64(relop, "EstimatedTotalSubtreeCost"),
        self_cost: 0.0,
        act_time_ms,
        runtime,
        warnings,
        props,
        children: child_ops.into_iter().map(parse_relop).collect(),
    }
}

fn operator_element<'a, 'i>(relop: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    relop.children().rfind(|n| n.is_element() && !RELOP_METADATA.contains(&n.tag_name().name()))
}

/// Nearest RelOp descendants: the operator's inputs.
fn collect_relops<'a, 'i>(node: Node<'a, 'i>, out: &mut Vec<Node<'a, 'i>>) {
    for c in node.children().filter(Node::is_element) {
        if c.has_tag_name_local("RelOp") {
            out.push(c);
        } else {
            collect_relops(c, out);
        }
    }
}

fn node_kind(op: &str, logical: &str) -> NodeKind {
    let op_l = op.to_ascii_lowercase();
    let logical_l = logical.to_ascii_lowercase();
    if op_l.contains("lookup") {
        return NodeKind::Lookup;
    }
    if op_l.contains("insert")
        || op_l.contains("update")
        || op_l.contains("delete")
        || logical_l == "merge" && !op_l.contains("join")
    {
        return NodeKind::Modify;
    }
    if op_l.contains("seek") {
        return NodeKind::Seek;
    }
    if op_l == "constant scan" {
        return NodeKind::Other;
    }
    if op_l.contains("scan") {
        return NodeKind::Scan;
    }
    if op_l.contains("spool") {
        return NodeKind::Spool;
    }
    if op_l.contains("sort") {
        return NodeKind::Sort;
    }
    if op_l.contains("aggregate") || logical_l.contains("aggregate") || logical_l == "distinct" {
        return NodeKind::Aggregate;
    }
    if op_l.contains("nested loops") || op_l.contains("join") || op_l.contains("hash match") {
        return NodeKind::Join;
    }
    if op_l == "parallelism" {
        return NodeKind::Parallelism;
    }
    if op_l.contains("compute scalar") {
        return NodeKind::Compute;
    }
    NodeKind::Other
}

fn object_of(operator: Node) -> Option<(Option<String>, Option<String>)> {
    let object = child(operator, "Object")?;
    let schema = object.attribute("Schema").map(unbracket);
    let table = object.attribute("Table").map(unbracket);
    let qualified = match (schema, table) {
        (Some(s), Some(t)) => Some(format!("{s}.{t}")),
        (None, Some(t)) => Some(t),
        _ => None,
    };
    Some((qualified, object.attribute("Index").map(unbracket)))
}

fn seek_predicate_of(operator: Node) -> Option<String> {
    // Index Spools keep their keys in SeekPredicateNew directly, without the SeekPredicates wrapper.
    let seeks = child(operator, "SeekPredicates").or_else(|| child(operator, "SeekPredicateNew"))?;
    let mut parts = Vec::new();
    for keys in seeks.descendants().filter(|n| n.has_tag_name_local("SeekKeys")) {
        for range in keys.children().filter(Node::is_element) {
            let scan_type = range.attribute("ScanType").unwrap_or("EQ");
            let columns: Vec<String> = child(range, "RangeColumns")
                .map(|rc| children(rc, "ColumnReference").map(column_name).collect())
                .unwrap_or_default();
            let values: Vec<String> = child(range, "RangeExpressions")
                .map(|re| {
                    children(re, "ScalarOperator")
                        .map(|s| s.attribute("ScalarString").unwrap_or("?").to_string())
                        .collect()
                })
                .unwrap_or_default();
            for (i, column) in columns.iter().enumerate() {
                let value = values.get(i).map(String::as_str).unwrap_or("?");
                parts.push(format!("{column} {} {value}", comparison(scan_type)));
            }
        }
    }
    if parts.is_empty() {
        let fallback: Vec<&str> = seeks
            .descendants()
            .filter(|n| n.has_tag_name_local("ScalarOperator"))
            .filter_map(|n| n.attribute("ScalarString"))
            .collect();
        if fallback.is_empty() {
            return None;
        }
        return Some(truncate(fallback.join(" AND ")));
    }
    Some(truncate(parts.join(" AND ")))
}

fn predicate_of(operator: Node) -> Option<String> {
    let element = child(operator, "Predicate")
        .or_else(|| child(operator, "ProbeResidual"))
        .or_else(|| child(operator, "Residual"))?;
    let scalar = element.descendants().find(|n| n.has_tag_name_local("ScalarOperator"))?;
    scalar.attribute("ScalarString").map(|s| truncate(s.to_string()))
}

fn comparison(scan_type: &str) -> &'static str {
    match scan_type {
        "GT" => ">",
        "GE" => ">=",
        "LT" => "<",
        "LE" => "<=",
        "NE" => "<>",
        "IS" => "IS",
        "IS NOT" => "IS NOT",
        _ => "=",
    }
}

fn column_name(column_ref: Node) -> String {
    let column = column_ref.attribute("Column").unwrap_or("?");
    format!("[{}]", unbracket(column))
}

type Counters = (Option<f64>, Option<f64>, Option<f64>, Option<NodeRuntime>);

/// Totals across threads: rows, executions, reads and CPU add up; elapsed is the slowest thread.
fn runtime_counters(rt: Node, props: &mut Properties) -> Counters {
    let mut rows = 0.0;
    let mut executions = 0.0;
    let mut rows_read = 0.0;
    let mut elapsed: Option<f64> = None;
    let mut seen = false;
    let mut runtime = NodeRuntime::default();
    let add = |total: &mut Option<f64>, value: Option<f64>| {
        if let Some(v) = value {
            *total = Some(total.unwrap_or(0.0) + v);
        }
    };
    let opt = |node: Node, name: &str| node.attribute(name).and_then(|v| v.parse::<f64>().ok());
    // Worker threads (thread 0 coordinates a parallel zone): how evenly the rows were spread.
    let mut workers = 0.0;
    let mut busiest: f64 = 0.0;
    for thread in children(rt, "RunTimeCountersPerThread") {
        seen = true;
        let thread_rows = attr_f64(thread, "ActualRows");
        rows += thread_rows;
        if thread.attribute("Thread").is_some_and(|t| t != "0") {
            workers += 1.0;
            busiest = busiest.max(thread_rows);
        }
        executions += attr_f64(thread, "ActualExecutions");
        rows_read += attr_f64(thread, "ActualRowsRead");
        if let Some(ms) = opt(thread, "ActualElapsedms") {
            elapsed = Some(elapsed.map_or(ms, |e: f64| e.max(ms)));
        }
        add(&mut runtime.cpu_ms, opt(thread, "ActualCPUms"));
        add(&mut runtime.logical_reads, opt(thread, "ActualLogicalReads"));
        add(&mut runtime.physical_reads, opt(thread, "ActualPhysicalReads"));
        add(&mut runtime.physical_reads, opt(thread, "ActualReadAheads"));
        // Thread 0 coordinates a parallel plan; the workers are 1..n.
        if attr_f64(thread, "Thread") > 0.0 {
            runtime.thread_rows.push(thread_rows);
        }
        if let Some(mode) = thread.attribute("ActualExecutionMode") {
            props.insert("ActualExecutionMode".into(), mode.into());
        }
    }
    if !seen {
        return (None, None, None, None);
    }
    if runtime.thread_rows.len() < 2 {
        runtime.thread_rows.clear();
    }
    props.insert("ActualRows".into(), rows.to_string());
    props.insert("ActualExecutions".into(), executions.to_string());
    if workers >= 2.0 {
        props.insert("ThreadCount".into(), workers.to_string());
        props.insert("ThreadRowsMax".into(), busiest.to_string());
    }
    if rows_read > 0.0 {
        props.insert("ActualRowsRead".into(), rows_read.to_string());
    }
    (Some(rows), Some(executions), elapsed, (!runtime.is_empty()).then_some(runtime))
}

/// `Seek Plan: CONVERT_IMPLICIT(nvarchar(255),[db].[dbo].[orders].[status],0)=[@P1]`, one per line.
fn convert_issues(warnings: Node) -> Option<String> {
    let lines: Vec<String> = children(warnings, "PlanAffectingConvert")
        .map(|w| {
            format!("{}: {}", w.attribute("ConvertIssue").unwrap_or("Convert"), w.attribute("Expression").unwrap_or(""))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn warnings_of(warnings: Node) -> Vec<PlanWarning> {
    let mut out = Vec::new();
    if warnings.attribute("NoJoinPredicate").is_some_and(is_true) {
        push_unique(&mut out, PlanWarning::NoJoinPredicate);
    }
    for w in warnings.children().filter(Node::is_element) {
        let warning = match w.tag_name().name() {
            "PlanAffectingConvert" => Some(PlanWarning::ImplicitConversion),
            "SpillToTempDb" | "SortSpillDetails" | "HashSpillDetails" | "ExchangeSpillDetails" | "SpillOccurred" => {
                Some(PlanWarning::Spill)
            }
            "ColumnsWithNoStatistics" => Some(PlanWarning::MissingStatistics),
            "MemoryGrantWarning" => Some(PlanWarning::MemoryGrant),
            "NoJoinPredicate" => Some(PlanWarning::NoJoinPredicate),
            _ => None,
        };
        if let Some(warning) = warning {
            push_unique(&mut out, warning);
        }
    }
    out
}

/// Flags operators whose actual row count is at least 10× away from the estimate, on plans with runtime data.
fn flag_misestimates(node: &mut PlanNode) {
    const FACTOR: f64 = 10.0;
    const MIN_ROWS: f64 = 100.0;
    if let Some(m) = node.misestimate() {
        let larger = node.act_rows.unwrap_or(0.0).max(node.est_rows_total());
        if m.abs() >= FACTOR && larger >= MIN_ROWS {
            push_unique(&mut node.warnings, PlanWarning::Misestimate);
        }
    }
    node.children.iter_mut().for_each(flag_misestimates);
}

fn index_hints(query_plan: Node) -> Vec<PlanIndexHint> {
    let Some(missing) = child(query_plan, "MissingIndexes") else { return Vec::new() };
    let mut hints = Vec::new();
    for group in children(missing, "MissingIndexGroup") {
        let impact = attr_f64(group, "Impact");
        for index in children(group, "MissingIndex") {
            let schema = index.attribute("Schema").map(unbracket).unwrap_or_default();
            let table = index.attribute("Table").map(unbracket).unwrap_or_default();
            let mut hint = PlanIndexHint {
                table: if schema.is_empty() { table } else { format!("{schema}.{table}") },
                equality: Vec::new(),
                inequality: Vec::new(),
                include: Vec::new(),
                impact_pct: impact,
            };
            for group in children(index, "ColumnGroup") {
                let columns: Vec<String> =
                    children(group, "Column").filter_map(|c| c.attribute("Name")).map(unbracket).collect();
                match group.attribute("Usage") {
                    Some("EQUALITY") => hint.equality = columns,
                    Some("INEQUALITY") => hint.inequality = columns,
                    Some("INCLUDE") => hint.include = columns,
                    _ => {}
                }
            }
            hints.push(hint);
        }
    }
    hints
}

trait LocalName {
    fn has_tag_name_local(&self, name: &str) -> bool;
}

impl LocalName for Node<'_, '_> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name_local(name))
}

fn children<'a, 'i: 'a>(node: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    node.children().filter(move |n| n.has_tag_name_local(name))
}

fn attr_f64(node: Node, name: &str) -> f64 {
    node.attribute(name).and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

fn copy_attrs(node: Node, names: &[&str], props: &mut Properties) {
    for name in names {
        if let Some(v) = node.attribute(*name) {
            props.insert((*name).to_string(), v.to_string());
        }
    }
}

fn is_true(v: &str) -> bool {
    v == "1" || v.eq_ignore_ascii_case("true")
}

fn unbracket(s: &str) -> String {
    s.trim().trim_start_matches('[').trim_end_matches(']').to_string()
}

fn truncate(s: String) -> String {
    if s.chars().count() <= MAX_LIST_PROP_LEN {
        return s;
    }
    let mut out: String = s.chars().take(MAX_LIST_PROP_LEN).collect();
    out.push('…');
    out
}

fn push_unique(list: &mut Vec<PlanWarning>, warning: PlanWarning) {
    if !list.contains(&warning) {
        list.push(warning);
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn statement_types_list_every_statement_the_server_sees() {
        let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements>
            <StmtSimple StatementText="SELECT 1" StatementType="SELECT WITHOUT QUERY"/>
            <StmtSimple StatementText="COMMIT" StatementType="COMMIT TRANSACTION"/>
            <StmtCond StatementText="IF 1 = 1" StatementType="COND"><Condition/><Then><Statements>
              <StmtSimple StatementText="CREATE TABLE t (a int)" StatementType="CREATE TABLE"/>
            </Statements></Then></StmtCond>
        </Statements></Batch></BatchSequence></ShowPlanXML>"#;
        assert_eq!(
            statement_types(xml).unwrap(),
            vec!["SELECT WITHOUT QUERY", "COMMIT TRANSACTION", "COND", "CREATE TABLE"]
        );
    }

    use super::*;

    const ESTIMATED: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.564" Build="16.0.4135.4">
  <BatchSequence><Batch><Statements>
    <StmtSimple StatementText="SELECT m.Id, m.Body FROM dbo.Orders m WHERE m.CustomerId = @p1 AND m.Kind = N'x'" StatementId="1" StatementCompId="1" StatementType="SELECT" StatementSubTreeCost="12.5" StatementEstRows="40" StatementOptmLevel="FULL" QueryHash="0xAB" QueryPlanHash="0xCD" CardinalityEstimationModelVersion="160">
      <QueryPlan DegreeOfParallelism="1" CachedPlanSize="40" CompileTime="3" CompileCPU="3" CompileMemory="400">
        <Warnings><PlanAffectingConvert ConvertIssue="Seek Plan" Expression="CONVERT_IMPLICIT(nvarchar(36),[m].[CustomerId],0)=[@p1]" /></Warnings>
        <MissingIndexes>
          <MissingIndexGroup Impact="97.5">
            <MissingIndex Database="[shop]" Schema="[dbo]" Table="[Orders]">
              <ColumnGroup Usage="EQUALITY"><Column Name="[CustomerId]" ColumnId="2" /></ColumnGroup>
              <ColumnGroup Usage="INCLUDE"><Column Name="[Body]" ColumnId="3" /></ColumnGroup>
            </MissingIndex>
          </MissingIndexGroup>
        </MissingIndexes>
        <ParameterList><ColumnReference Column="@p1" ParameterCompiledValue="N'abc'" /></ParameterList>
        <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="40" EstimateIO="0" EstimateCPU="0.0002" AvgRowSize="100" EstimatedTotalSubtreeCost="12.5" Parallel="0" EstimateRebinds="0" EstimateRewinds="0" EstimatedExecutionMode="Row">
          <OutputList><ColumnReference Database="[shop]" Schema="[dbo]" Table="[Orders]" Alias="[m]" Column="Id" /></OutputList>
          <NestedLoops Optimized="0">
            <OuterReferences><ColumnReference Column="Id" /></OuterReferences>
            <RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="40" EstimatedRowsRead="1800000" TableCardinality="1800000" EstimateIO="8" EstimateCPU="2" AvgRowSize="50" EstimatedTotalSubtreeCost="10" Parallel="0" EstimateRebinds="0" EstimateRewinds="0" EstimatedExecutionMode="Row">
              <OutputList />
              <IndexScan Ordered="0" ForcedIndex="0" ForceScan="0" NoExpandHint="0" Storage="RowStore">
                <DefinedValues />
                <Object Database="[shop]" Schema="[dbo]" Table="[Orders]" Index="[PK_Orders]" Alias="[m]" IndexKind="Clustered" Storage="RowStore" />
                <Predicate><ScalarOperator ScalarString="CONVERT_IMPLICIT(nvarchar(36),[shop].[dbo].[Orders].[CustomerId] as [m].[CustomerId],0)=[@p1]" /></Predicate>
              </IndexScan>
            </RelOp>
            <RelOp NodeId="2" PhysicalOp="Clustered Index Seek" LogicalOp="Clustered Index Seek" EstimateRows="1" EstimateIO="0.003" EstimateCPU="0.0001" AvgRowSize="4000" EstimatedTotalSubtreeCost="2.4" Parallel="0" EstimateRebinds="39" EstimateRewinds="0" EstimatedExecutionMode="Row">
              <OutputList><ColumnReference Column="Body" /></OutputList>
              <IndexScan Lookup="1" Ordered="1" ScanDirection="FORWARD" ForcedIndex="0" ForceSeek="0" ForceScan="0" NoExpandHint="0" Storage="RowStore">
                <DefinedValues />
                <Object Database="[shop]" Schema="[dbo]" Table="[Orders]" Index="[PK_Orders]" Alias="[m]" TableReferenceId="-1" IndexKind="Clustered" Storage="RowStore" />
                <SeekPredicates>
                  <SeekPredicateNew>
                    <SeekKeys>
                      <Prefix ScanType="EQ">
                        <RangeColumns><ColumnReference Database="[shop]" Schema="[dbo]" Table="[Orders]" Alias="[m]" Column="Id" /></RangeColumns>
                        <RangeExpressions><ScalarOperator ScalarString="[shop].[dbo].[Orders].[Id] as [m].[Id]" /></RangeExpressions>
                      </Prefix>
                    </SeekKeys>
                  </SeekPredicateNew>
                </SeekPredicates>
              </IndexScan>
            </RelOp>
          </NestedLoops>
        </RelOp>
      </QueryPlan>
    </StmtSimple>
  </Statements></Batch></BatchSequence>
</ShowPlanXML>"#;

    #[test]
    fn parses_an_estimated_plan() {
        let plan = parse(ESTIMATED).unwrap();
        assert_eq!(plan.kind, PlanKind::Estimated);
        assert_eq!(plan.total_cost, 12.5);
        assert!(plan.statement.as_deref().unwrap().starts_with("SELECT m.Id"));
        assert_eq!(plan.warnings, vec![PlanWarning::ImplicitConversion]);
        assert_eq!(plan.props.get("Parameters").map(String::as_str), Some("@p1 = N'abc'"));

        let root = &plan.root;
        assert_eq!(root.op, "Nested Loops");
        assert_eq!(root.detail.as_deref(), Some("Inner Join"));
        assert_eq!(root.kind, NodeKind::Join);
        assert_eq!(root.children.len(), 2);
        assert!((root.self_cost - 0.1).abs() < 1e-9);

        let scan = &root.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert_eq!(scan.object.as_deref(), Some("dbo.Orders"));
        assert_eq!(scan.index.as_deref(), Some("PK_Orders"));
        assert_eq!(scan.rows_read, Some(1_800_000.0));
        assert!(scan.predicate.as_deref().unwrap().contains("CONVERT_IMPLICIT"));
        assert!(scan.warnings.contains(&PlanWarning::ResidualPredicate));
        assert_eq!(plan.hottest().id, 1);

        let lookup = &root.children[1];
        assert_eq!(lookup.op, "Key Lookup");
        assert_eq!(lookup.kind, NodeKind::Lookup);
        assert_eq!(lookup.est_executions, 40.0);
        assert_eq!(lookup.seek_predicate.as_deref(), Some("[Id] = [shop].[dbo].[Orders].[Id] as [m].[Id]"));
        assert_eq!(lookup.props.get("OutputList").map(String::as_str), Some("Body"));

        assert_eq!(plan.index_hints.len(), 1);
        let hint = &plan.index_hints[0];
        assert_eq!(hint.table, "dbo.Orders");
        assert_eq!(hint.equality, vec!["CustomerId"]);
        assert_eq!(hint.include, vec!["Body"]);
        assert_eq!(hint.impact_pct, 97.5);
    }

    #[test]
    fn reads_runtime_counters_from_actual_plans() {
        let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements>
          <StmtSimple StatementText="SELECT 1" StatementSubTreeCost="1" QueryHash="0xAB12">
            <QueryPlan>
              <ThreadStat Branches="1" UsedThreads="2" />
              <MemoryGrantInfo SerialDesiredMemory="1024" GrantedMemory="1024" MaxUsedMemory="1024" />
              <QueryTimeStats CpuTime="80" ElapsedTime="61" />
              <RelOp NodeId="0" PhysicalOp="Sort" LogicalOp="Sort" EstimateRows="10" EstimatedTotalSubtreeCost="1">
                <OutputList />
                <Warnings>
                  <SpillToTempDb SpillLevel="1" SpilledThreadCount="1" />
                  <SortSpillDetails GrantedMemoryKb="1024" UsedMemoryKb="1024" WritesToTempDb="96" ReadsFromTempDb="96" />
                </Warnings>
                <RunTimeInformation>
                  <RunTimeCountersPerThread Thread="1" ActualRows="6000" ActualExecutions="1" ActualElapsedms="40" ActualCPUms="38" ActualLogicalReads="10" ActualPhysicalReads="1" ActualReadAheads="2" />
                  <RunTimeCountersPerThread Thread="2" ActualRows="4000" ActualExecutions="1" ActualElapsedms="55" ActualCPUms="41" ActualLogicalReads="12" ActualPhysicalReads="0" ActualReadAheads="0" />
                </RunTimeInformation>
                <Sort Distinct="0" />
              </RelOp>
            </QueryPlan>
          </StmtSimple>
        </Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let plan = parse(xml).unwrap();
        assert_eq!(plan.kind, PlanKind::Actual);
        let sort = &plan.root;
        assert_eq!(sort.kind, NodeKind::Sort);
        assert_eq!(sort.act_rows, Some(10_000.0));
        assert_eq!(sort.act_executions, Some(2.0));
        assert_eq!(sort.act_time_ms, Some(55.0));
        assert!(sort.warnings.contains(&PlanWarning::Spill));
        assert!(sort.warnings.contains(&PlanWarning::Misestimate));
        let runtime = sort.runtime.as_ref().unwrap();
        assert_eq!(runtime.cpu_ms, Some(79.0));
        assert_eq!(runtime.logical_reads, Some(22.0));
        assert_eq!(runtime.physical_reads, Some(3.0));
        assert_eq!(runtime.thread_rows, vec![6000.0, 4000.0]);
        assert_eq!(runtime.spill_pages, Some(96.0));
        assert_eq!(plan.props.get("ElapsedMs").map(String::as_str), Some("61"));
        assert_eq!(plan.props.get("CpuMs").map(String::as_str), Some("80"));
        assert_eq!(plan.props.get("UsedThreads").map(String::as_str), Some("2"));
        let summary = culprit_core::actual_summary(&plan).unwrap();
        assert_eq!((summary.elapsed_ms, summary.cpu_ms, summary.spilled), (Some(61.0), Some(80.0), true));
    }

    #[test]
    fn picks_the_statement_a_query_hash_names() {
        let batch = r#"<ShowPlanXML><BatchSequence><Batch><Statements>
          <StmtSimple StatementText="SELECT big" StatementSubTreeCost="9" QueryHash="0x01">
            <QueryPlan><RelOp NodeId="0" PhysicalOp="Table Scan" LogicalOp="Table Scan" EstimateRows="1" EstimatedTotalSubtreeCost="9"><TableScan /></RelOp></QueryPlan>
          </StmtSimple>
          <StmtSimple StatementText="SELECT small" StatementSubTreeCost="1" QueryHash="0x02">
            <QueryPlan><RelOp NodeId="0" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="1"><IndexScan /></RelOp></QueryPlan>
          </StmtSimple>
        </Statements></Batch></BatchSequence></ShowPlanXML>"#;
        assert_eq!(parse(batch).unwrap().statement.as_deref(), Some("SELECT big"));
        assert_eq!(parse_statement(batch, Some("0X02")).unwrap().statement.as_deref(), Some("SELECT small"));
        assert_eq!(parse_statement(batch, Some("0x99")).unwrap().statement.as_deref(), Some("SELECT big"));
    }

    #[test]
    fn rejects_documents_without_plans() {
        assert!(parse("<ShowPlanXML/>").is_err());
        assert!(parse("not xml").is_err());
    }
}
