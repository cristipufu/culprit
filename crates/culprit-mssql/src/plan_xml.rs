use culprit_core::insights::EXCHANGE_SPILL;
use culprit_core::{
    Engine, Error, NodeKind, NodeRuntime, PARALLEL_THREADS, Plan, PlanIndexHint, PlanKind, PlanNode, PlanWarning,
    Properties, Result,
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
    "EstimateRowsWithoutRowGoal",
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

/// Query Store also keeps valid empty showplans for maintenance such as UPDATE STATISTICS. They provide no
/// operator tree; that is absent plan data, not a parser failure. Malformed or nonempty unsupported XML is not empty.
pub(crate) fn is_empty(xml: &str) -> bool {
    let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
    let Ok(doc) = Document::parse_with_options(xml, options) else { return false };
    doc.root_element().has_tag_name_local("ShowPlanXML")
        && doc.descendants().any(|n| n.has_tag_name_local("Statements"))
        && doc
            .descendants()
            .filter(|n| n.has_tag_name_local("Statements"))
            .all(|n| !n.children().any(|c| c.is_element()))
        && doc
            .descendants()
            .filter(|n| n.is_element())
            .all(|n| matches!(n.tag_name().name(), "ShowPlanXML" | "BatchSequence" | "Batch" | "Statements"))
}

/// A plan a scan saved, read again from its XML with this parser, so a scan saved by an older Culprit reads the way
/// one taken today would: the same statement of the document (by the `QueryHash` the saved plan carries). `None` when
/// the plan kept no XML, or this parser can't read it.
pub fn reparse(saved: &Plan) -> Option<Plan> {
    parse_statement(saved.raw.as_deref()?, saved.facts().query_hash.as_deref()).ok()
}

/// Parse the statement whose `QueryHash` matches (a batch or procedure plan holds every statement), or the most
/// expensive statement when none matches.
pub fn parse_statement(xml: &str, query_hash: Option<&str>) -> Result<Plan> {
    let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
    let doc = Document::parse_with_options(xml, options)
        .map_err(|e| Error::internal(format!("Could not read the SQL Server plan XML: {e}")))?;
    fn query_plan_of<'a, 'input>(n: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
        if n.has_tag_name_local("StmtSimple") {
            child(n, "QueryPlan")
        } else if n.has_tag_name_local("StmtCond") {
            child(n, "Condition").and_then(|c| child(c, "QueryPlan"))
        } else {
            None
        }
    }
    let candidates = || doc.descendants().filter(|n| query_plan_of(*n).is_some());
    let matching = query_hash.and_then(|hash| {
        candidates().find(|n| n.attribute("QueryHash").is_some_and(|h| h.eq_ignore_ascii_case(hash.trim())))
    });
    let statement = matching
        .or_else(|| {
            candidates()
                .max_by(|a, b| attr_f64(*a, "StatementSubTreeCost").total_cmp(&attr_f64(*b, "StatementSubTreeCost")))
        })
        .ok_or_else(|| Error::internal("The plan XML contains no statement with a query plan"))?;
    let query_plan = query_plan_of(statement).expect("filtered on QueryPlan above");
    let root_op = child(query_plan, "RelOp").ok_or_else(|| Error::internal("The query plan has no operators"))?;

    let mut root = parse_relop(root_op);
    let total_cost =
        statement.attribute("StatementSubTreeCost").and_then(|v| v.parse().ok()).unwrap_or(root.subtree_cost);
    let is_actual = query_plan.descendants().any(|n| n.has_tag_name_local("RunTimeInformation"));

    let mut props = Properties::new();
    copy_attrs(statement, STATEMENT_PROPS, &mut props);
    copy_attrs(query_plan, QUERY_PLAN_PROPS, &mut props);
    if let Some(grant) = child(query_plan, "MemoryGrantInfo") {
        for (attr, key) in [
            ("SerialDesiredMemory", "MemoryDesiredKB"),
            ("RequestedMemory", "MemoryRequestedKB"),
            ("GrantedMemory", "MemoryGrantedKB"),
            ("MaxUsedMemory", "MemoryMaxUsedKB"),
        ] {
            if let Some(v) = grant.attribute(attr) {
                props.insert(key.into(), v.into());
            }
        }
        // The showplan schema gives the wait for a memory grant in seconds.
        if let Some(seconds) = grant.attribute("GrantWaitTime").and_then(|v| v.parse::<f64>().ok()) {
            props.insert("MemoryGrantWaitMs".into(), (seconds * 1000.0).to_string());
        }
    }
    if let Some(time) = child(query_plan, "QueryTimeStats") {
        // Time in scalar user-defined functions is counted apart (SQL Server 2016 SP2+): the plan's costs leave it out.
        for (attr, key) in [
            ("ElapsedTime", "ElapsedMs"),
            ("CpuTime", "CpuMs"),
            ("UdfElapsedTime", "UdfElapsedMs"),
            ("UdfCpuTime", "UdfCpuMs"),
        ] {
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
        root.flag_misestimates();
    }

    let mut plan = Plan {
        facts: Some(statement_facts(statement, query_plan)),
        view: None,
        engine: Engine::Mssql,
        kind: if is_actual { PlanKind::Actual } else { PlanKind::Estimated },
        statement: statement.attribute("StatementText").map(|s| s.trim().to_string()),
        total_cost,
        root,
        warnings,
        index_hints: index_hints(query_plan),
        props,
        raw: Some(xml.into()),
        insights: None,
    };
    plan.compute_self_costs();
    Ok(plan)
}

fn statement_facts(statement: Node, plan: Node) -> culprit_core::PlanFacts {
    use culprit_core::plan_facts::{count, nonnegative};
    let number = |node: Node, key: &str| node.attribute(key).and_then(|v| v.parse::<f64>().ok()).and_then(nonnegative);
    let grant = child(plan, "MemoryGrantInfo");
    let memory = |key: &str| grant.and_then(|n| number(n, key));
    let time = child(plan, "QueryTimeStats");
    let timing = |key: &str| time.and_then(|n| number(n, key));
    culprit_core::PlanFacts {
        query_hash: statement.attribute("QueryHash").map(str::to_string),
        parallelism_blocker: plan.attribute("NonParallelPlanReason").map(str::to_string),
        optimization_early_abort: statement
            .attribute("StatementOptmEarlyAbortReason")
            .or_else(|| plan.attribute("StatementOptmEarlyAbortReason"))
            .map(str::to_string),
        compiled_parameters: child(plan, "ParameterList").and_then(|params| {
            let list: Vec<String> = children(params, "ColumnReference")
                .map(|p| {
                    let name = p.attribute("Column").unwrap_or("?");
                    p.attribute("ParameterCompiledValue").map_or_else(|| name.into(), |v| format!("{name} = {v}"))
                })
                .collect();
            (!list.is_empty()).then(|| list.join(", "))
        }),
        conversions: child(plan, "Warnings")
            .map(|w| {
                children(w, "PlanAffectingConvert")
                    .map(|w| culprit_core::PlanConversion {
                        issue: culprit_core::PlanConversionIssue::from_source(
                            w.attribute("ConvertIssue").unwrap_or("Convert"),
                        ),
                        expression: w.attribute("Expression").unwrap_or("").into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        memory_grant_warning: child(plan, "Warnings")
            .and_then(|w| child(w, "MemoryGrantWarning"))
            .and_then(|g| g.attribute("GrantWarningKind"))
            .map(str::to_string),
        elapsed_ms: timing("ElapsedTime"),
        cpu_ms: timing("CpuTime"),
        compile_ms: number(plan, "CompileTime"),
        compile_cpu_ms: number(plan, "CompileCPU"),
        compile_memory_kb: number(plan, "CompileMemory"),
        degree_of_parallelism: number(plan, "DegreeOfParallelism").and_then(count),
        used_threads: child(plan, "ThreadStat").and_then(|n| number(n, "UsedThreads")).and_then(count),
        cardinality_estimator: number(statement, "CardinalityEstimationModelVersion").and_then(count),
        memory_desired_kb: memory("SerialDesiredMemory"),
        memory_requested_kb: memory("RequestedMemory"),
        memory_granted_kb: memory("GrantedMemory"),
        memory_max_used_kb: memory("MaxUsedMemory"),
        memory_grant_wait_ms: memory("GrantWaitTime").and_then(|seconds| nonnegative(seconds * 1000.0)),
        udf_elapsed_ms: timing("UdfElapsedTime"),
        udf_cpu_ms: timing("UdfCpuTime"),
        ..Default::default()
    }
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
            props.insert("OutputList".into(), columns.join(", "));
        }
    }

    // Scalar functions this operator calls (not those of operators below it): each one runs once per row, except in a
    // seek's keys, which are worked out once per seek.
    let calls: Vec<(String, bool)> = relop
        .descendants()
        .filter(|n| n.has_tag_name_local("UserDefinedFunction"))
        .filter(|n| n.ancestors().skip(1).find(|a| a.has_tag_name_local("RelOp")) == Some(relop))
        .filter_map(|n| {
            let in_seek = n
                .ancestors()
                .skip(1)
                .take_while(|a| *a != relop)
                .any(|a| a.has_tag_name_local("SeekPredicates") || a.has_tag_name_local("SeekPredicateNew"));
            Some((n.attribute("FunctionName")?, in_seek))
        })
        .map(|(name, in_seek)| {
            // [db].[schema].[function] → schema.function
            let parts: Vec<String> = name.split("].[").map(unbracket).collect();
            (parts[parts.len().saturating_sub(2)..].join("."), in_seek)
        })
        .collect();
    if !calls.is_empty() {
        let functions: Vec<&str> = calls.iter().map(|(f, _)| f.as_str()).collect();
        props.insert("UserDefinedFunctions".into(), truncate(functions.join(", ")));
        if calls.iter().all(|(_, in_seek)| *in_seek) {
            props.insert(culprit_core::plan::UDF_IN_SEEK_ONLY.into(), "true".into());
        }
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
        // An exchange spills its buffers to resolve a deadlock between parallel threads, not for lack of memory.
        if child(w, "ExchangeSpillDetails").is_some() {
            props.insert(EXCHANGE_SPILL.into(), "true".into());
        }
    }
    if let Some(columns) = child(relop, "Warnings").and_then(|w| child(w, "ColumnsWithNoStatistics")) {
        let names: Vec<String> = children(columns, "ColumnReference").map(column_name).map(|c| unbracket(&c)).collect();
        if !names.is_empty() {
            props.insert("ColumnsWithNoStatistics".into(), names.join(", "));
        }
    }

    let (object, index) = operator.and_then(object_of).unwrap_or((None, None));
    let seek_predicate = operator.and_then(seek_predicate_of);
    let predicate = operator.and_then(predicate_of);

    let est_rows = attr_f64(relop, "EstimateRows");
    let est_executions = 1.0 + attr_f64(relop, "EstimateRebinds") + attr_f64(relop, "EstimateRewinds");
    let table_rows = relop.attribute("TableCardinality").and_then(|v| v.parse().ok());

    let mut facts = culprit_core::PlanNodeFacts {
        estimated_row_bytes: relop
            .attribute("AvgRowSize")
            .and_then(|v| v.parse::<f64>().ok())
            .and_then(culprit_core::plan_facts::nonnegative),
        output_columns: child(relop, "OutputList").map(|o| {
            children(o, "ColumnReference").filter_map(|c| c.attribute("Column")).map(str::to_string).collect()
        }),
        storage: operator.and_then(|o| o.attribute("Storage")).map(str::to_string),
        conversions: child(relop, "Warnings")
            .map(|w| {
                children(w, "PlanAffectingConvert")
                    .map(|w| culprit_core::PlanConversion {
                        issue: culprit_core::PlanConversionIssue::from_source(
                            w.attribute("ConvertIssue").unwrap_or("Convert"),
                        ),
                        expression: w.attribute("Expression").unwrap_or("").into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        many_to_many: operator.and_then(|o| o.attribute("ManyToMany")).map(is_true),
        table_alias: operator.and_then(|o| child(o, "Object")).and_then(|o| o.attribute("Alias")).map(unbracket),
        user_functions: calls.iter().map(|(name, _)| name.clone()).collect(),
        functions_in_seek_only: (!calls.is_empty()).then(|| calls.iter().all(|(_, seek)| *seek)),
        columns_without_statistics: child(relop, "Warnings")
            .and_then(|w| child(w, "ColumnsWithNoStatistics"))
            .map(|cols| children(cols, "ColumnReference").map(column_name).map(|c| unbracket(&c)).collect())
            .unwrap_or_default(),
        estimated_execution_mode: relop
            .attribute("EstimatedExecutionMode")
            .map(culprit_core::PlanExecutionMode::from_source),
        estimated_rows_without_goal: relop
            .attribute("EstimateRowsWithoutRowGoal")
            .and_then(|v| v.parse::<f64>().ok())
            .and_then(culprit_core::plan_facts::nonnegative),
        exchange_spill: child(relop, "Warnings").and_then(|w| child(w, "ExchangeSpillDetails")).map(|_| true),
        ..Default::default()
    };
    let Counters { rows: act_rows, executions: act_executions, elapsed_ms: act_time_ms, rows_read, mut runtime } =
        child(relop, "RunTimeInformation").map(|rt| runtime_counters(rt, &mut props, &mut facts)).unwrap_or_default();
    // Sort/hash/exchange spill details say how many pages went to tempdb.
    let spilled: f64 = child(relop, "Warnings")
        .map(|w| {
            w.descendants().filter_map(|d| d.attribute("WritesToTempDb")).filter_map(|v| v.parse::<f64>().ok()).sum()
        })
        .unwrap_or(0.0);
    if spilled > 0.0 {
        runtime.get_or_insert_with(NodeRuntime::default).spill_pages = Some(spilled);
    }

    let warnings = child(relop, "Warnings").map(warnings_of).unwrap_or_default();
    let mut child_ops = Vec::new();
    if let Some(o) = operator {
        collect_relops(o, &mut child_ops);
    }
    let estimated_read = relop.attribute("EstimatedRowsRead").and_then(|v| v.parse::<f64>().ok());

    let mut node = PlanNode {
        facts: Some(facts),
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
        rows_read: None,
        table_rows,
        subtree_cost: attr_f64(relop, "EstimatedTotalSubtreeCost"),
        self_cost: 0.0,
        act_time_ms,
        runtime,
        warnings,
        props,
        children: child_ops.into_iter().map(parse_relop).collect(),
    };
    // Rows read across every execution, like `act_rows`: counted by SQL Server when it did (ActualRowsRead), else its
    // estimate per execution times the executions (the measured ones in an actual plan).
    node.rows_read = rows_read.or_else(|| estimated_read.map(|per_run| per_run * node.runs()));
    let returned = node.act_rows.unwrap_or_else(|| node.est_rows_total());
    if node.predicate.is_some()
        && node
            .rows_read
            .is_some_and(|read| read >= RESIDUAL_MIN_ROWS_READ && read >= RESIDUAL_READ_FACTOR * returned.max(1.0))
    {
        push_unique(&mut node.warnings, PlanWarning::ResidualPredicate);
    }
    node
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
        return Some(fallback.join(" AND "));
    }
    Some(parts.join(" AND "))
}

fn predicate_of(operator: Node) -> Option<String> {
    let element = child(operator, "Predicate")
        .or_else(|| child(operator, "ProbeResidual"))
        .or_else(|| child(operator, "Residual"))?;
    let scalar = element.descendants().find(|n| n.has_tag_name_local("ScalarOperator"))?;
    // Predicates feed semantic analysis; only display properties may be shortened.
    scalar.attribute("ScalarString").map(str::to_string)
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

#[derive(Default)]
struct Counters {
    rows: Option<f64>,
    executions: Option<f64>,
    elapsed_ms: Option<f64>,
    rows_read: Option<f64>,
    runtime: Option<NodeRuntime>,
}

/// Totals across threads: rows, executions, reads and CPU add up; elapsed is the slowest thread.
fn runtime_counters(rt: Node, props: &mut Properties, facts: &mut culprit_core::PlanNodeFacts) -> Counters {
    let mut rows = 0.0;
    let mut executions = 0.0;
    // Where SQL Server counted it: a measured 0 (an empty table, a seek that found nothing) is a count, not a gap.
    let mut rows_read: Option<f64> = None;
    let (mut rebinds, mut rewinds): (Option<f64>, Option<f64>) = (None, None);
    let mut threads_run = 0.0;
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
        let thread_executions = attr_f64(thread, "ActualExecutions");
        executions += thread_executions;
        if thread_executions > 0.0 && thread.attribute("Thread").is_some_and(|t| t != "0") {
            threads_run += 1.0;
        }
        add(&mut rows_read, opt(thread, "ActualRowsRead"));
        // Spools: a rebind runs the input again, a rewind replays the rows the spool already holds.
        add(&mut rebinds, opt(thread, "ActualRebinds"));
        add(&mut rewinds, opt(thread, "ActualRewinds"));
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
            facts.actual_execution_mode = Some(culprit_core::PlanExecutionMode::from_source(mode));
            props.insert("ActualExecutionMode".into(), mode.into());
        }
    }
    if !seen {
        return Counters::default();
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
    if let Some(read) = rows_read {
        props.insert("ActualRowsRead".into(), read.to_string());
    }
    facts.parallel_threads = culprit_core::plan_facts::thread_count(threads_run);
    if threads_run >= 2.0 {
        props.insert(PARALLEL_THREADS.into(), threads_run.to_string());
    }
    for (key, value) in [("ActualRebinds", rebinds), ("ActualRewinds", rewinds)] {
        if let Some(v) = value {
            props.insert(key.into(), v.to_string());
        }
    }
    Counters {
        rows: Some(rows),
        executions: Some(executions),
        elapsed_ms: elapsed,
        rows_read,
        runtime: (!runtime.is_empty()).then_some(runtime),
    }
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
            // SQL Server warns about every conversion that may sway the plan, including ones the query writes itself.
            "PlanAffectingConvert" => w
                .attribute("Expression")
                .is_some_and(culprit_core::insights::converts_column)
                .then_some(PlanWarning::ImplicitConversion),
            // A sort or hash that ran out of memory. An exchange spill is told apart (`EXCHANGE_SPILL`): it resolves a
            // deadlock between parallel threads, and memory advice doesn't apply to it.
            "SpillToTempDb" | "SortSpillDetails" | "HashSpillDetails" | "SpillOccurred" => Some(PlanWarning::Spill),
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
    fn semantic_predicates_keep_conversions_after_long_prefixes() {
        let prefix = (0..35).map(|i| format!("[m].[field_{i}]=[@p{i}] AND ")).collect::<String>();
        let predicate = format!("{prefix}CONVERT_IMPLICIT(nvarchar(36),[m].[CustomerId],0)=[@p1]");
        assert!(prefix.len() > 600);
        let xml = ESTIMATED.replace(
            "CONVERT_IMPLICIT(nvarchar(36),[shop].[dbo].[Orders].[CustomerId] as [m].[CustomerId],0)=[@p1]",
            &predicate,
        );
        let p = super::parse(&xml).expect("plan");
        let saved = p.nodes().into_iter().find_map(|n| n.predicate.as_deref()).expect("predicate");
        assert_eq!(saved, predicate);
        assert!(!saved.ends_with('…'));
    }

    #[test]
    fn empty_maintenance_showplans_are_absent_data_not_parse_failures() {
        let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements /></Batch></BatchSequence></ShowPlanXML>"#;
        assert!(super::is_empty(xml));
        assert!(!super::is_empty("<ShowPlanXML>"), "malformed XML remains a parser failure");
        assert!(!super::is_empty("<something><Statements /></something>"));
        assert!(!super::is_empty("<ShowPlanXML><Statements><StmtSimple /></Statements></ShowPlanXML>"));
        assert!(!super::is_empty("<ShowPlanXML><Statements><Unknown /></Statements></ShowPlanXML>"));
        assert!(!super::is_empty("<ShowPlanXML><Statements /><Unknown /></ShowPlanXML>"));
        assert!(!super::is_empty("<ShowPlanXML><Statements /><RelOp /></ShowPlanXML>"));
        assert!(!super::is_empty("<ShowPlanXML />"), "a missing expected structure is not a known empty plan");
    }

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

    #[test]
    fn a_conditional_plan_keeps_its_own_operators_hash_and_measurements() {
        let xml = r#"<ShowPlanXML><BatchSequence><Batch><Statements>
          <StmtCond StatementText="IF NOT EXISTS (SELECT 1 FROM dbo.orders)" QueryHash="0x01" StatementSubTreeCost="1">
            <Condition><QueryPlan CompileTime="2"><RelOp NodeId="0" PhysicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="1"><IndexScan><Object Schema="[dbo]" Table="[orders]" Index="[pk_orders]" /></IndexScan></RelOp></QueryPlan></Condition>
            <Then><Statements><StmtSimple StatementText="SELECT payload FROM dbo.orders" QueryHash="0x02" StatementSubTreeCost="9">
              <QueryPlan><RelOp NodeId="1" PhysicalOp="Table Scan" EstimateRows="10" EstimatedTotalSubtreeCost="9"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="10" ActualExecutions="1" /></RunTimeInformation><TableScan><Object Schema="[dbo]" Table="[orders]" /></TableScan></RelOp></QueryPlan>
            </StmtSimple></Statements></Then>
          </StmtCond>
        </Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let condition = parse_statement(xml, Some("0X01")).unwrap();
        assert_eq!(condition.root.op, "Index Seek");
        assert_eq!(condition.root.object.as_deref(), Some("dbo.orders"));
        assert_eq!(condition.kind, PlanKind::Estimated, "a branch's measurements do not belong to the condition");
        assert_eq!(condition.total_cost, 1.0);
        assert_eq!(condition.props.get("CompileTime").map(String::as_str), Some("2"));
        assert!(condition.statement.as_deref().unwrap().starts_with("IF NOT EXISTS"));
        assert_eq!(reparse(&condition).unwrap().root.op, "Index Seek");
        assert_eq!(parse(xml).unwrap().root.op, "Table Scan", "the default still picks the most costly statement");
        assert_eq!(parse_statement(xml, Some("0x02")).unwrap().kind, PlanKind::Actual);
    }

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
    fn conversions_the_query_writes_are_not_a_type_mismatch() {
        // SQL Server warns about each CONVERT in a select list; the plan keeps the text, but nothing is mismatched.
        let written = ESTIMATED.replace(
            r#"<PlanAffectingConvert ConvertIssue="Seek Plan" Expression="CONVERT_IMPLICIT(nvarchar(36),[m].[CustomerId],0)=[@p1]" />"#,
            r#"<PlanAffectingConvert ConvertIssue="Cardinality Estimate" Expression="CONVERT(varchar(36),[m].[Id],0)" />"#,
        );
        let plan = parse(&written).unwrap();
        assert!(!plan.warnings.contains(&PlanWarning::ImplicitConversion));
        assert_eq!(
            plan.props.get("PlanAffectingConvert").map(String::as_str),
            Some("Cardinality Estimate: CONVERT(varchar(36),[m].[Id],0)")
        );
    }

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
        assert_eq!(plan.hottest().unwrap().id, 1);

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
    fn statement_compiler_and_output_facts_survive_removing_display_properties() {
        let mut plan = parse(ESTIMATED).unwrap();
        assert_eq!(plan.facts().compiled_parameters.as_deref(), Some("@p1 = N'abc'"));
        assert_eq!(plan.facts().query_hash.as_deref(), Some("0xAB"));
        assert_eq!(plan.root.children[1].facts().output_columns, Some(vec!["Body".into()]));
        assert_eq!(plan.root.children[0].facts().output_columns, Some(vec![]));
        assert_eq!(culprit_core::signals::plan_row_bytes(&plan), Some(100.0));
        let expected = serde_json::to_value(culprit_core::insights::insights(&plan)).unwrap();
        fn clear(n: &mut PlanNode) {
            n.props.clear();
            for c in &mut n.children {
                clear(c);
            }
        }
        clear(&mut plan.root);
        plan.props.clear();
        assert_eq!(serde_json::to_value(culprit_core::insights::insights(&plan)).unwrap(), expected);
        assert_eq!(culprit_core::signals::plan_row_bytes(&plan), Some(100.0));
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
        let mut plan = parse(xml).unwrap();
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
        assert_eq!(plan.root.facts().parallel_threads, Some(2));
        plan.root.props.remove(PARALLEL_THREADS);
        assert_eq!(plan.root.runs(), 1.0, "native thread facts normalize executions without the display property");
        plan.props.clear();
        let summary = culprit_core::actual_summary(&plan).unwrap();
        assert_eq!((summary.elapsed_ms, summary.cpu_ms, summary.workers), (Some(61.0), Some(80.0), Some(2.0)));
        assert_eq!((summary.memory_granted_kb, summary.memory_used_kb), (Some(1024.0), Some(1024.0)));
    }

    #[test]
    fn actual_counts_are_totals_and_units_follow_the_schema() {
        let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements>
          <StmtSimple StatementText="SELECT 1" StatementSubTreeCost="20">
            <QueryPlan>
              <MemoryGrantInfo GrantedMemory="1024" MaxUsedMemory="1024" GrantWaitTime="2" />
              <QueryTimeStats CpuTime="900" ElapsedTime="700" UdfCpuTime="500" UdfElapsedTime="450" />
              <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="1000" EstimatedTotalSubtreeCost="20">
                <OutputList />
                <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1000" ActualExecutions="1" /></RunTimeInformation>
                <NestedLoops Optimized="0">
                  <RelOp NodeId="1" PhysicalOp="Parallelism" LogicalOp="Gather Streams" EstimateRows="1000" EstimatedTotalSubtreeCost="5">
                    <OutputList />
                    <Warnings><ExchangeSpillDetails WritesToTempDb="640" /></Warnings>
                    <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1000" ActualExecutions="1" /></RunTimeInformation>
                    <Parallelism>
                      <RelOp NodeId="2" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="1000" EstimatedRowsRead="500000" TableCardinality="500000" Parallel="1" EstimatedTotalSubtreeCost="4">
                        <OutputList />
                        <RunTimeInformation>
                          <RunTimeCountersPerThread Thread="1" ActualRows="400" ActualRowsRead="250000" ActualExecutions="1" />
                          <RunTimeCountersPerThread Thread="2" ActualRows="600" ActualRowsRead="250000" ActualExecutions="1" />
                        </RunTimeInformation>
                        <IndexScan><Object Database="[db]" Schema="[dbo]" Table="[t]" Index="[pk]" />
                          <Predicate><ScalarOperator ScalarString="[db].[dbo].[t].[a]=(1)" /></Predicate></IndexScan>
                      </RelOp>
                    </Parallelism>
                  </RelOp>
                  <RelOp NodeId="3" PhysicalOp="Table Spool" LogicalOp="Lazy Spool" EstimateRows="50" EstimateRebinds="0" EstimateRewinds="999" EstimatedTotalSubtreeCost="10">
                    <OutputList />
                    <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="50000" ActualExecutions="1000" ActualRebinds="1" ActualRewinds="999" /></RunTimeInformation>
                    <Spool>
                      <RelOp NodeId="4" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="50" EstimatedRowsRead="80" EstimatedTotalSubtreeCost="1">
                        <OutputList />
                        <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="0" ActualExecutions="0" /></RunTimeInformation>
                        <IndexScan><Object Database="[db]" Schema="[dbo]" Table="[u]" Index="[ix]" /></IndexScan>
                      </RelOp>
                    </Spool>
                  </RelOp>
                </NestedLoops>
              </RelOp>
            </QueryPlan>
          </StmtSimple>
        </Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let plan = parse(xml).unwrap();
        // The showplan schema gives GrantWaitTime in seconds.
        assert_eq!(plan.props.get("MemoryGrantWaitMs").map(String::as_str), Some("2000"));
        assert_eq!(plan.props.get("UdfElapsedMs").map(String::as_str), Some("450"));
        let gather = &plan.root.children[0];
        // An exchange spill isn't a sort or hash short of memory.
        assert!(!gather.warnings.contains(&PlanWarning::Spill));
        assert_eq!(gather.props.get(EXCHANGE_SPILL).map(String::as_str), Some("true"));
        let scan = &gather.children[0];
        // Rows read is the measured total across threads, like actual rows; the scan ran once on each of two threads,
        // one execution as the estimate counts it, and returned the 1,000 rows it expected.
        assert_eq!(scan.rows_read, Some(500_000.0));
        assert_eq!(scan.props.get(PARALLEL_THREADS).map(String::as_str), Some("2"));
        assert_eq!(scan.runs(), 1.0);
        assert!(!scan.warnings.contains(&PlanWarning::Misestimate));
        assert!(scan.warnings.contains(&PlanWarning::ResidualPredicate));
        let spool = &plan.root.children[1];
        assert_eq!(spool.props.get("ActualRebinds").map(String::as_str), Some("1"));
        assert_eq!(spool.props.get("ActualRewinds").map(String::as_str), Some("999"));
        // Never ran: nothing to compare, and nothing read.
        let never = &spool.children[0];
        assert_eq!(never.runs(), 0.0);
        assert_eq!(never.rows_read, Some(0.0));
        assert!(!never.warnings.contains(&PlanWarning::Misestimate));

        // An estimated plan: EstimatedRowsRead is per execution, so the total multiplies by the executions.
        let estimated = parse(ESTIMATED).unwrap();
        let lookup = &estimated.root.children[1];
        assert_eq!(lookup.est_executions, 40.0);
        assert_eq!(estimated.root.children[0].rows_read, Some(1_800_000.0));
    }

    #[test]
    fn a_measured_zero_rows_read_is_kept() {
        // The scan ran and read nothing (the table is empty now); its estimate of 500,000 rows read is no count.
        let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements>
          <StmtSimple StatementText="SELECT 1" StatementSubTreeCost="4">
            <QueryPlan>
              <RelOp NodeId="0" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="1000" EstimatedRowsRead="500000" TableCardinality="500000" EstimatedTotalSubtreeCost="4">
                <OutputList />
                <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="0" ActualRowsRead="0" ActualExecutions="1" /></RunTimeInformation>
                <IndexScan><Object Database="[db]" Schema="[dbo]" Table="[t]" Index="[pk]" />
                  <Predicate><ScalarOperator ScalarString="[db].[dbo].[t].[a]=(1)" /></Predicate></IndexScan>
              </RelOp>
            </QueryPlan>
          </StmtSimple>
        </Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let scan = parse(xml).unwrap().root;
        assert_eq!(scan.rows_read, Some(0.0));
        assert!(!scan.warnings.contains(&PlanWarning::ResidualPredicate), "nothing read, nothing thrown away");
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
