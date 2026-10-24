//! Plan tree, `PlanFacts`, `PlanNodeFacts`, and native plan format (Showplan XML, EXPLAIN JSON, text) redaction.

use crate::model::{Engine, Properties};
use crate::plan::{Plan, PlanNode};

use super::sql::{redact_prose, redact_sql, redact_strings, strip_database, strip_parts};

/// Units a plan writes after a measured number: MySQL's `data_read_per_join` ("106M", "1G"), sizes ("8kB", "4MB"),
/// times ("0.5ms") and shares ("10%").
const MEASUREMENT_UNITS: &[&str] = &[
    "K", "M", "G", "T", "P", "E", "B", "KB", "kB", "MB", "GB", "TB", "KiB", "MiB", "GiB", "TiB", "ms", "us", "s", "%",
];

/// A value that is a measurement (cost, rows, a size, a flag), not text that could hold data. A whole value that is a
/// number with a unit is the plan's own figure: a literal from the statement is never only that.
pub(super) fn is_measurement(value: &str) -> bool {
    let v = value.trim();
    let unit_at = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    let (number, unit) = v.split_at(unit_at);
    v.parse::<f64>().is_ok()
        || (number.parse::<f64>().is_ok() && MEASUREMENT_UNITS.contains(&unit.trim_start()))
        || matches!(v.to_ascii_lowercase().as_str(), "true" | "false" | "yes" | "no")
}

fn redact_props(props: &Properties, engine: Engine) -> Properties {
    props
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                if is_measurement(v) {
                    v.clone()
                } else {
                    let value = redact_sql(v, engine);
                    if engine == Engine::Mssql { strip_database(&value) } else { value }
                },
            )
        })
        .collect()
}

fn redact_plan_expression(text: &str) -> String {
    let mut prefix = "__culprit_plan_reference_".to_string();
    while text.contains(&prefix) {
        prefix.push('_');
    }
    let mut mapped = String::with_capacity(text.len());
    let mut rest = text;
    let mut refs = Vec::new();
    while let Some(at) = rest.find("SubPlan ") {
        let start = at + "SubPlan ".len();
        let digits = rest[start..].chars().take_while(char::is_ascii_digit).count();
        mapped.push_str(&rest[..at]);
        if digits == 0 || rest[..at].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            mapped.push_str("SubPlan ");
            rest = &rest[start..];
            continue;
        }
        let original = &rest[at..start + digits];
        let token = format!("{prefix}{}END", refs.len());
        mapped.push_str(&token);
        refs.push((token, original.to_string()));
        rest = &rest[start + digits..];
    }
    mapped.push_str(rest);
    let mut masked = redact_sql(&mapped, Engine::Postgres);
    for (token, original) in refs {
        masked = masked.replace(&token, &original);
    }
    masked
}

/// The plan with literal values replaced in its statement, predicates and text properties. The original plan text
/// (`raw`) is dropped; use [`redact_native`] for that.
pub fn redact_plan(plan: &Plan) -> Plan {
    fn text(t: &str, engine: Engine) -> String {
        let t = if engine.sql_syntax() == crate::SqlSyntax::Postgres {
            redact_plan_expression(t)
        } else {
            redact_sql(t, engine)
        };
        if engine == Engine::Mssql { strip_database(&t) } else { t }
    }
    fn node(n: &PlanNode, engine: Engine) -> PlanNode {
        let facts = n.facts.as_ref().map(|facts| {
            let mut f = facts.clone();
            f.selectivity_guess = f.selectivity_guess.as_deref().map(|v| redact_prose(v, engine));
            f.partition_pruning = f.partition_pruning.as_deref().map(|v| redact_prose(v, engine));
            f.join_filter = f.join_filter.as_deref().map(|v| text(v, engine));
            f.storage = f.storage.as_deref().map(|v| redact_strings(v, engine));
            f.join_buffer = f.join_buffer.as_deref().map(|v| redact_strings(v, engine));
            f.join_buffer_join_type = f.join_buffer_join_type.as_deref().map(|v| redact_strings(v, engine));
            f.index_method = f.index_method.as_deref().map(|v| redact_strings(v, engine));
            f.aggregate_strategy = f.aggregate_strategy.as_deref().map(|v| redact_strings(v, engine));
            f.conditions = f.conditions.iter().map(|v| text(v, engine)).collect();
            f.columns_without_statistics = f.columns_without_statistics.iter().map(|v| text(v, engine)).collect();
            f.output_columns = f.output_columns.as_ref().map(|values| values.iter().map(|v| text(v, engine)).collect());
            f.user_functions = f
                .user_functions
                .iter()
                .map(|v| {
                    let value = text(v, engine);
                    if engine == Engine::Mssql { strip_parts(&value, 2) } else { value }
                })
                .collect();
            for c in &mut f.conversions {
                c.expression = text(&c.expression, engine);
                if let crate::PlanConversionIssue::Other(v) = &mut c.issue {
                    *v = redact_strings(v, engine);
                }
            }
            if let Some(crate::PlanParentRelationship::Other(v)) = &mut f.parent_relationship {
                *v = redact_strings(v, engine);
            }
            for mode in [&mut f.actual_execution_mode, &mut f.estimated_execution_mode].into_iter().flatten() {
                if let crate::PlanExecutionMode::Other(v) = mode {
                    *v = redact_strings(v, engine);
                }
            }
            f
        });
        PlanNode {
            facts,
            detail: n.detail.as_deref().map(|d| text(d, engine)),
            seek_predicate: n.seek_predicate.as_deref().map(|p| text(p, engine)),
            predicate: n.predicate.as_deref().map(|p| text(p, engine)),
            props: n
                .props
                .iter()
                .map(|(k, v)| (k.clone(), if is_measurement(v) { v.clone() } else { text(v, engine) }))
                .collect(),
            children: n.children.iter().map(|c| node(c, engine)).collect(),
            ..n.clone()
        }
    }
    let mut redacted = Plan {
        view: None,
        statement: plan.statement.as_deref().map(|s| redact_sql(s, plan.engine)),
        root: node(&plan.root, plan.engine),
        props: redact_props(&plan.props, plan.engine),
        raw: None,
        insights: None,
        ..plan.clone()
    };
    if let Some(f) = &mut redacted.facts {
        f.flavor = f.flavor.as_deref().map(|v| redact_strings(v, plan.engine));
        f.server_version = f.server_version.as_deref().map(|v| redact_strings(v, plan.engine));
        f.memory_grant_warning = f.memory_grant_warning.as_deref().map(|v| redact_strings(v, plan.engine));
        f.compiled_parameters = f.compiled_parameters.as_deref().map(|v| text(v, plan.engine));
        for c in &mut f.conversions {
            c.expression = text(&c.expression, plan.engine);
            if let crate::PlanConversionIssue::Other(v) = &mut c.issue {
                *v = redact_strings(v, plan.engine);
            }
        }
        f.generic_plan_reason = f.generic_plan_reason.as_deref().map(|v| redact_prose(v, plan.engine));
        f.cost_basis = f.cost_basis.as_deref().map(|v| redact_prose(v, plan.engine));
        f.parallelism_blocker = f.parallelism_blocker.as_deref().map(|v| redact_strings(v, plan.engine));
        f.optimization_early_abort = f.optimization_early_abort.as_deref().map(|v| redact_strings(v, plan.engine));
        f.statistics_maintenance = f.statistics_maintenance.as_deref().map(|v| text(v, plan.engine));
    }
    // Insights quote the predicates they were found in: worked out again from the redacted plan.
    if plan.insights.is_some() {
        crate::insights::annotate(&mut redacted);
        // Recomputing from a redacted tree must not restore urgency or unsafe index SQL that the selected scan
        // withheld. Keep primary redaction by deriving prose from the redacted tree; catalog-specific fixes can't
        // be reconstructed from a standalone tree, so those require the original scan's catalog again.
        if let (Some(source), Some(target)) = (&plan.insights, &mut redacted.insights) {
            let original = crate::insights::insights(plan);
            let mut contextual = source.note != crate::insights::coverage_note(plan);
            for i in &mut target.items {
                if let Some(before) =
                    source.items.iter().find(|b| b.rule == i.rule && b.nodes == i.nodes && b.index_hint == i.index_hint)
                {
                    i.severity = i.severity.min(before.severity);
                    i.fix_sql = before.fix_sql.as_deref().map(|sql| redact_sql(sql, plan.engine));
                    if original.iter().any(|raw| {
                        raw.rule == before.rule
                            && raw.nodes == before.nodes
                            && raw.index_hint == before.index_hint
                            && raw.fix != before.fix
                    }) {
                        contextual = true;
                        if i.area == crate::insights::InsightArea::Index {
                            i.fix = "Check this observation against the selected scan's workload and current index definitions before changing an index. Prefer narrow projections and balance saved reads against storage and write cost; a full covering index is not automatically worthwhile.".into();
                        }
                        i.verify = "Compare a newly compiled plan, reads and time under equivalent parameters and load. Stored warnings and a changing share of database time alone do not verify improvement.".into();
                    }
                }
            }
            if contextual {
                let context = "Workload priority and SQL restrictions from the selected scan are retained. Historical or cheap statements can still have structural scaling risks; consult the selected scan for catalog-specific advice.";
                target.note =
                    Some(target.note.as_deref().map_or_else(|| context.to_string(), |n| format!("{n} {context}")));
            }
        }
    }
    redacted
}

/// Showplan XML attributes that carry SQL text or values.
const SHOWPLAN_TEXT: &[&str] = &[
    "StatementText",
    "ScalarString",
    "ParameterCompiledValue",
    "ParameterRuntimeValue",
    "ConstValue",
    "ParameterizedText",
    "Expression",
];

/// The plan's original text (a .sqlplan, EXPLAIN JSON or text) with literal values redacted, keeping its format so it
/// opens in the engine's own tools.
pub fn redact_native(raw: &str, engine: Engine) -> String {
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('<') {
        return redact_showplan(raw);
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw)
    {
        redact_json(&mut value, engine);
        return serde_json::to_string_pretty(&value).unwrap_or_default();
    }
    raw.lines().map(|line| redact_text_line(line, engine)).collect::<Vec<_>>().join("\n")
}

fn redact_showplan(xml: &str) -> String {
    // Wrap fragments too (some tools copy individual Showplan elements). Leave the original declaration in place
    // when writing the result, but not inside the wrapper used to parse attributes and their exact source ranges.
    let trimmed = xml.trim_start_matches('\u{feff}').trim_start();
    let offset = if trimmed.strip_prefix("<?xml").is_some_and(|rest| rest.starts_with(char::is_whitespace)) {
        let Some(end) = trimmed.find("?>") else { return redaction_failed() };
        let decl_end = xml.len() - trimmed.len() + end + 2;
        let decl_probe = format!("{}<culprit/>", &xml[..decl_end]);
        let Ok(decl_doc) = roxmltree::Document::parse(&decl_probe) else { return redaction_failed() };
        if decl_doc.descendants().any(|n| n.is_pi() || n.is_comment()) {
            return redaction_failed();
        }
        decl_end
    } else {
        0
    };
    const PREFIX: &str = "<culprit>";
    let wrapped = format!("{PREFIX}{}</culprit>", &xml[offset..]);
    let Ok(doc) = roxmltree::Document::parse(&wrapped) else { return redaction_failed() };
    let mut replacements = Vec::new();
    for node in doc.descendants() {
        if node.is_pi() {
            return redaction_failed();
        }
        if node.is_comment() {
            replacements.push((node.range(), "<!-- … -->".to_string()));
        }
        if node.is_text()
            && let Some(text) = node.text().filter(|t| !t.trim().is_empty())
        {
            replacements.push((node.range(), xml_escape(&strip_database(&redact_sql(text, Engine::Mssql)))));
        }
        for attr in node.attributes() {
            let replacement = match attr.name() {
                "Database" => Some("[database]".to_string()),
                "Server" => Some("[server]".to_string()),
                name if SHOWPLAN_TEXT.contains(&name) => {
                    Some(xml_escape(&strip_database(&redact_sql(attr.value(), Engine::Mssql))))
                }
                _ => None,
            };
            if let Some(replacement) = replacement {
                replacements.push((attr.range_value(), replacement));
            }
        }
    }
    replacements.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(xml.len());
    let mut copied = 0;
    for (range, replacement) in replacements {
        let start = range.start - PREFIX.len() + offset;
        let end = range.end - PREFIX.len() + offset;
        out.push_str(&xml[copied..start]);
        out.push_str(&replacement);
        copied = end;
    }
    out.push_str(&xml[copied..]);
    out
}

fn redaction_failed() -> String {
    "<!-- Culprit could not safely redact this XML plan. -->".into()
}

fn redact_json(value: &mut serde_json::Value, engine: Engine) {
    match value {
        serde_json::Value::String(s) if !is_measurement(s) => *s = redact_sql(s, engine),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| redact_json(v, engine)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| redact_json(v, engine)),
        _ => {}
    }
}

/// Postgres text EXPLAIN and MySQL tree output: what comes before the `(cost=…)`/`(actual …)` figures, and `Key: value`
/// detail lines, may hold literals; the figures are measurements and stay.
fn redact_text_line(line: &str, engine: Engine) -> String {
    let figures = ["(cost=", "(actual ", "(never executed)", "(rows="].iter().filter_map(|m| line.find(m)).min();
    if let Some(at) = figures {
        return format!("{}{}", redact_sql(&line[..at], engine), &line[at..]);
    }
    let body = line.trim_start();
    if let Some((key, value)) = body.split_once(": ")
        && !key.is_empty()
        && key.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '/')
    {
        const MEASURES: &[&str] = &[
            "Buffers",
            "Planning",
            "Planning Time",
            "Execution Time",
            "Workers Planned",
            "Workers Launched",
            "Sort Method",
            "Heap Blocks",
            "Batches",
            "Memory Usage",
            "I/O Timings",
            "JIT",
            "Functions",
            "Options",
            "Timing",
            "Rows Removed by Filter",
            "Rows Removed by Index Recheck",
            "Rows Removed by Join Filter",
            "Heap Fetches",
            "Peak Memory Usage",
            "Disk Usage",
            "Cache Hits",
            "Cache Misses",
            "Cache Evictions",
            "Hits",
            "Misses",
            "Evictions",
        ];
        if MEASURES.iter().any(|m| m.eq_ignore_ascii_case(key.trim())) {
            return line.to_string();
        }
        let indent = &line[..line.len() - body.len()];
        return format!("{indent}{key}: {}", redact_sql(value, engine));
    }
    redact_strings(line, engine)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}
