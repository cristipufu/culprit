//! Captured facts used in plan calculations. Provider properties remain available for inspection; their labels are
//! interpreted only by the compatibility adapter for plans saved before these fields existed.
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::Properties;

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[serde(default)]
#[ts(export)]
pub struct PlanFacts {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub elapsed_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub cpu_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub planning_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub compile_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub compile_cpu_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub compile_memory_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub memory_desired_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub memory_requested_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub memory_granted_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub memory_max_used_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub memory_grant_wait_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub udf_elapsed_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub udf_cpu_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub degree_of_parallelism: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub used_threads: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub cardinality_estimator: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub cost_threshold_for_parallelism: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub server_maxdop: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub database_maxdop: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub jit_time_ms: Option<f64>,
    #[serde(default, with = "trigger_measurements")]
    #[ts(as = "Vec<crate::actual::TriggerTiming>")]
    pub trigger_timings: Vec<crate::actual::TriggerTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism_blocker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimization_early_abort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generic_plan_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statistics_maintenance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled_parameters: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_grant_warning: Option<String>,
    pub conversions: Vec<PlanConversion>,
    pub changed_columns: Vec<PlanChangedColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub docdb: Option<crate::signals::DocdbMetrics>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[serde(default)]
#[ts(export)]
pub struct PlanNodeFacts {
    // Number of threads whose execution counts contribute to this operator's total, not requested parallelism.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub parallel_threads: Option<u32>,
    // Statement measurements attributable to this scan, from preparation. A fraction in 0..=1, without rounding.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "fraction_number")]
    #[ts(as = "Option<f64>")]
    pub measured_kept_share: Option<f64>,
    // MySQL/MariaDB's own estimate, in percent. A fixed optimizer guess remains distinguishable from a measurement.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "percentage_number")]
    #[ts(as = "Option<f64>")]
    pub estimated_filtered_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selectivity_guess: Option<String>,
    // Why a listed partition may not be read by an execution. Its catalog size cannot establish rows read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_pruning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_parent: Option<String>,
    // The catalog's native table kind. Managed storage can require different DDL from ordinary engine tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub table_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_relationship: Option<PlanParentRelationship>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_execution_mode: Option<PlanExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_execution_mode: Option<PlanExecutionMode>,
    // PostgreSQL's original per-process estimate, before Culprit normalizes the parallel subtree.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub estimated_rows_per_process: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub disk_usage_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub sort_space_kb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub hash_batches: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub hash_aggregate_batches: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub heap_fetches: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub lossy_heap_blocks: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub exact_heap_blocks: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub rows_removed_by_recheck: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub estimated_rows_without_goal: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub workers_planned: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub workers_launched: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "count_number")]
    #[ts(as = "Option<u32>")]
    pub sort_merge_passes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_on_disk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_spill: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_buffer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_buffer_join_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subplan_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_missing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub many_to_many: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anti_join: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cacheable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub functions_in_seek_only: Option<bool>,
    #[serde(default)]
    pub expression_cache: bool,
    pub conditions: Vec<String>,
    pub conversions: Vec<PlanConversion>,
    pub user_functions: Vec<String>,
    pub columns_without_statistics: Vec<String>,
    pub possible_index_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub possible_index_columns: Option<Vec<PlanIndexColumns>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_indexes: Option<Vec<PlanIndexColumns>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_index: Option<PlanAutomaticIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub estimated_row_bytes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nonnegative_number")]
    #[ts(as = "Option<f64>")]
    pub rows_produced_per_join: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_columns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub docdb: Option<crate::signals::DocdbMetrics>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanParentRelationship {
    Outer,
    Inner,
    SubPlan,
    InitPlan,
    Cte,
    Other(String),
}
impl PlanParentRelationship {
    pub fn from_source(value: &str) -> Self {
        match value {
            "Outer" => Self::Outer,
            "Inner" => Self::Inner,
            "SubPlan" => Self::SubPlan,
            "InitPlan" => Self::InitPlan,
            "CTE" => Self::Cte,
            other => Self::Other(other.into()),
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanExecutionMode {
    Row,
    Batch,
    Other(String),
}
impl PlanExecutionMode {
    pub fn from_source(value: &str) -> Self {
        if value.eq_ignore_ascii_case("row") {
            Self::Row
        } else if value.eq_ignore_ascii_case("batch") {
            Self::Batch
        } else {
            Self::Other(value.into())
        }
    }
}

/// Native numbers and older property strings use the same validity rules. Invalid evidence is unknown, never zero.
pub fn nonnegative(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value)
}
pub fn fraction(value: f64) -> Option<f64> {
    nonnegative(value).filter(|v| *v <= 1.0)
}
pub fn percentage(value: f64) -> Option<f64> {
    nonnegative(value).filter(|v| *v <= 100.0)
}
pub fn whole(value: f64) -> Option<f64> {
    nonnegative(value).filter(|v| v.fract() == 0.0)
}
pub fn thread_count(value: f64) -> Option<u32> {
    count(value).filter(|n| *n > 0)
}
pub fn count(value: f64) -> Option<u32> {
    (value.is_finite() && value >= 0.0 && value <= u32::MAX as f64 && value.fract() == 0.0).then_some(value as u32)
}

impl PlanFacts {
    pub fn from_legacy_properties(props: &Properties) -> Self {
        let number =
            |key: &str| props.get(key).and_then(|value| value.trim().parse::<f64>().ok()).and_then(nonnegative);
        let jit = props.get("JIT").and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let triggers = props.get("Triggers").and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        Self {
            docdb: None,
            query_hash: props.get("QueryHash").cloned(),
            query_identifier: props.get("Query Identifier").cloned(),
            parallelism_blocker: props.get("NonParallelPlanReason").cloned(),
            optimization_early_abort: props.get("StatementOptmEarlyAbortReason").cloned(),
            server_version: props.get("server_version").cloned(),
            flavor: props.get("flavor").cloned(),
            cost_basis: props.get("cost_basis").cloned(),
            generic_plan_reason: props.get("Generic Plan").cloned(),
            statistics_maintenance: props.get("Statistics maintenance").cloned(),
            compiled_parameters: props.get("Parameters").cloned(),
            memory_grant_warning: props.get("MemoryGrantWarning").cloned(),
            conversions: props
                .get("PlanAffectingConvert")
                .map(|v| v.lines().map(PlanConversion::from_description).collect())
                .unwrap_or_default(),
            changed_columns: props
                .get("Columns changed since capture")
                .map(|v| {
                    v.lines()
                        .filter_map(|line| {
                            let (name, ty) = line.split_once(' ')?;
                            let (table, column) = name.rsplit_once('.')?;
                            let (schema, table) =
                                table.rsplit_once('.').map_or((None, table), |(s, t)| (Some(s.to_string()), t));
                            Some(PlanChangedColumn {
                                schema,
                                table: table.into(),
                                column: column.into(),
                                data_type: ty.into(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            elapsed_ms: number("ElapsedMs").or_else(|| number("Execution Time")),
            cpu_ms: number("CpuMs"),
            planning_ms: number("PlanningMs").or_else(|| number("Planning Time")),
            compile_ms: number("CompileTime"),
            compile_cpu_ms: number("CompileCPU"),
            compile_memory_kb: number("CompileMemory"),
            memory_desired_kb: number("MemoryDesiredKB"),
            memory_requested_kb: number("MemoryRequestedKB"),
            memory_granted_kb: number("MemoryGrantedKB"),
            memory_max_used_kb: number("MemoryMaxUsedKB"),
            memory_grant_wait_ms: number("MemoryGrantWaitMs"),
            udf_elapsed_ms: number("UdfElapsedMs"),
            udf_cpu_ms: number("UdfCpuMs"),
            degree_of_parallelism: number("DegreeOfParallelism").and_then(count),
            used_threads: number("UsedThreads").and_then(count),
            cardinality_estimator: number("CardinalityEstimationModelVersion").and_then(count),
            cost_threshold_for_parallelism: number(crate::insights::COST_THRESHOLD),
            server_maxdop: number(crate::insights::SERVER_MAXDOP).and_then(count),
            database_maxdop: number(crate::insights::DATABASE_MAXDOP).and_then(count),
            jit_time_ms: jit
                .and_then(|v| v.pointer("/Timing/Total").and_then(serde_json::Value::as_f64))
                .and_then(nonnegative),
            trigger_timings: triggers
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|v| {
                    Some(crate::actual::TriggerTiming {
                        name: v.get("Trigger Name").and_then(serde_json::Value::as_str).unwrap_or("Trigger").into(),
                        relation: v.get("Relation").and_then(serde_json::Value::as_str).map(str::to_string),
                        time_ms: v.get("Time").and_then(serde_json::Value::as_f64).and_then(nonnegative)?,
                        calls: v.get("Calls").and_then(serde_json::Value::as_f64).and_then(whole),
                    })
                })
                .collect(),
        }
    }
}

impl PlanNodeFacts {
    /// The sole adapter for older normalized plans and external providers that still supply properties only. A
    /// present facts object is authoritative, including its unknown fields; it never inherits stale properties.
    pub fn from_legacy_properties(props: &Properties) -> Self {
        let number = |key: &str| props.get(key).and_then(|value| value.trim().parse::<f64>().ok());
        Self {
            docdb: None,
            table_kind: None,
            estimated_row_bytes: number("AvgRowSize").or_else(|| number("Plan Width")).and_then(nonnegative),
            rows_produced_per_join: number("rows_produced_per_join").and_then(nonnegative),
            output_columns: props.get("OutputList").map(|v| {
                v.split(',')
                    .map(|c| c.trim().rsplit('.').next().unwrap_or_default().trim_matches(['[', ']', '"']).to_string())
                    .collect()
            }),
            storage: props.get("Storage").cloned(),
            join_buffer: props.get("using_join_buffer").cloned(),
            join_buffer_join_type: props.get("join_buffer_join_type").cloned(),
            table_alias: props.get("table_name").or_else(|| props.get("Alias")).cloned(),
            join_filter: props.get("Join Filter").cloned(),
            subplan_name: props.get("Subplan Name").cloned(),
            aggregate_strategy: props.get("Strategy").cloned(),
            table_missing: props.get("Table not in the catalog").cloned(),
            many_to_many: props.get("ManyToMany").and_then(|v| legacy_bool(v)),
            anti_join: props.get("not_exists").and_then(|v| legacy_bool(v)),
            dependent: props.get("dependent").and_then(|v| legacy_bool(v)),
            cacheable: props.get("cacheable").and_then(|v| legacy_bool(v)),
            functions_in_seek_only: props.get("UserDefinedFunctionsInSeekOnly").and_then(|v| legacy_bool(v)),
            expression_cache: props.contains_key("cache_state"),
            conditions: ["Filter", "Join Filter", "Index Cond", "Recheck Cond", "Hash Cond", "Merge Cond"]
                .iter()
                .filter_map(|k| props.get(*k).cloned())
                .collect(),
            conversions: props
                .get("PlanAffectingConvert")
                .map(|v| v.lines().map(PlanConversion::from_description).collect())
                .unwrap_or_default(),
            user_functions: props
                .get("UserDefinedFunctions")
                .map(|v| v.split(", ").map(str::to_string).collect())
                .unwrap_or_default(),
            columns_without_statistics: props
                .get("ColumnsWithNoStatistics")
                .map(|v| v.split(", ").map(str::to_string).collect())
                .unwrap_or_default(),
            possible_index_names: props
                .get("possible_keys")
                .map(|v| v.split(", ").filter(|v| !v.is_empty()).map(str::to_string).collect())
                .unwrap_or_default(),
            possible_index_columns: props.get("possible_keys_columns").map(|v| legacy_indexes(v)),
            table_indexes: props.get(crate::insights::TABLE_INDEXES).map(|v| legacy_indexes(v)),
            automatic_index: props.get("Automatic Index").and_then(|v| PlanAutomaticIndex::from_description(v)),
            parallel_threads: number(crate::plan::PARALLEL_THREADS).and_then(thread_count),
            measured_kept_share: number(crate::plan::MEASURED_KEPT).and_then(fraction),
            estimated_filtered_percent: number("filtered").and_then(percentage),
            selectivity_guess: props.get(crate::plan::GUESSED_SELECTIVITY).cloned(),
            partition_pruning: props.get(crate::plan::NOT_READ_IN_FULL).cloned(),
            partition_parent: props.get("Partition Of").cloned(),
            index_method: props.get(crate::plan::INDEX_KIND).cloned(),
            parent_relationship: props
                .get("Parent Relationship")
                .map(|value| PlanParentRelationship::from_source(value)),
            actual_execution_mode: props.get("ActualExecutionMode").map(|value| PlanExecutionMode::from_source(value)),
            estimated_execution_mode: props
                .get("EstimatedExecutionMode")
                .map(|value| PlanExecutionMode::from_source(value)),
            disk_usage_kb: number("Disk Usage").and_then(nonnegative),
            sort_space_kb: number("Sort Space Used").and_then(nonnegative),
            hash_batches: number("Hash Batches").and_then(count),
            hash_aggregate_batches: number("HashAgg Batches").and_then(count),
            heap_fetches: number("Heap Fetches").and_then(nonnegative),
            lossy_heap_blocks: number("Lossy Heap Blocks").and_then(nonnegative),
            exact_heap_blocks: number("Exact Heap Blocks").and_then(nonnegative),
            rows_removed_by_recheck: number("Rows Removed by Index Recheck").and_then(nonnegative),
            estimated_rows_without_goal: number("EstimateRowsWithoutRowGoal").and_then(nonnegative),
            workers_planned: number("Workers Planned").and_then(count),
            workers_launched: number("Workers Launched").and_then(count),
            sort_merge_passes: number("r_sort_passes").and_then(count),
            sort_on_disk: props.get("Sort Space Type").and_then(|v| match v.as_str() {
                "Disk" => Some(true),
                "Memory" => Some(false),
                _ => None,
            }),
            exchange_spill: props.get(crate::insights::EXCHANGE_SPILL).and_then(|v| legacy_bool(v)),
            estimated_rows_per_process: number("Plan Rows per process").and_then(nonnegative),
        }
    }
}

// The wire remains an optional JSON number. Serde validates imported facts while TypeScript uses that same shape.
mod nonnegative_number {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.and_then(nonnegative))
    }
}
mod fraction_number {
    pub use super::nonnegative_number::serialize;
    use super::*;
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.and_then(fraction))
    }
}
mod percentage_number {
    pub use super::nonnegative_number::serialize;
    use super::*;
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.and_then(percentage))
    }
}

fn legacy_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Runtime counters are authoritative. Only older property-only plans need this compatibility path.
pub(crate) fn spill_pages(node: &crate::PlanNode) -> Option<f64> {
    node.runtime.as_ref().and_then(|r| r.spill_pages).and_then(nonnegative).or_else(|| {
        node.facts
            .is_none()
            .then(|| node.props.get("TempdbWritesPages")?.parse::<f64>().ok().and_then(nonnegative))
            .flatten()
    })
}

/// Number of participating workers/processes, their total rows and the busiest worker's rows. Native plans retain
/// the actual worker vector in NodeRuntime; old SQL Server plans retained only its maximum and count.
pub(crate) fn parallel_distribution(node: &crate::PlanNode, engine: crate::Engine) -> Option<(usize, f64, f64)> {
    if !matches!(engine, crate::Engine::Mssql | crate::Engine::Postgres) {
        return None;
    }
    let native = node.runtime.as_ref().map(|r| r.thread_rows.as_slice()).filter(|rows| rows.len() >= 2);
    let mut rows = if let Some(rows) = native {
        std::borrow::Cow::Borrowed(rows)
    } else if node.facts.is_none() && engine == crate::Engine::Postgres {
        let value: serde_json::Value = serde_json::from_str(node.props.get("Workers")?).ok()?;
        let rows = value
            .as_array()?
            .iter()
            .filter_map(|w| {
                let rows = w.get("Actual Rows")?.as_f64().and_then(nonnegative)?;
                let loops = w
                    .get("Actual Loops")
                    .and_then(serde_json::Value::as_f64)
                    .and_then(nonnegative)
                    .unwrap_or(1.0)
                    .max(1.0);
                nonnegative(rows * loops)
            })
            .collect::<Vec<_>>();
        std::borrow::Cow::Owned(rows)
    } else if node.facts.is_none() && engine == crate::Engine::Mssql {
        let number = |key: &str| node.props.get(key)?.parse::<f64>().ok().and_then(nonnegative);
        let threads = number("ThreadCount").and_then(count)?;
        let busiest = number("ThreadRowsMax")?;
        let total = nonnegative(node.act_rows?)?;
        return (threads >= 2 && busiest <= total).then_some((threads as usize, total, busiest));
    } else {
        return None;
    };
    if rows.is_empty() || rows.iter().any(|v| nonnegative(*v).is_none()) {
        return None;
    }
    if engine == crate::Engine::Postgres {
        let worker_total: f64 = rows.iter().sum();
        let leader = (node.act_rows.unwrap_or(worker_total) - worker_total).max(0.0);
        rows.to_mut().push(leader);
    }
    let total: f64 = rows.iter().sum();
    let busiest = rows.iter().copied().fold(0.0, f64::max);
    nonnegative(total)?;
    (rows.len() >= 2).then_some((rows.len(), total, busiest))
}

mod count_number {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &Option<u32>, s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.and_then(count))
    }
}

mod trigger_measurements {
    use super::*;
    #[derive(Deserialize)]
    struct Input {
        name: String,
        #[serde(default)]
        relation: Option<String>,
        #[serde(default)]
        time_ms: Option<f64>,
        #[serde(default)]
        calls: Option<f64>,
    }
    pub fn serialize<S: serde::Serializer>(value: &[crate::actual::TriggerTiming], s: S) -> Result<S::Ok, S::Error> {
        value.serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<crate::actual::TriggerTiming>, D::Error> {
        Ok(Vec::<Input>::deserialize(d)?
            .into_iter()
            .filter_map(|t| {
                Some(crate::actual::TriggerTiming {
                    name: t.name,
                    relation: t.relation,
                    time_ms: t.time_ms.and_then(nonnegative)?,
                    calls: t.calls.and_then(whole),
                })
            })
            .collect())
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct PlanIndexColumns {
    pub name: String,
    pub columns: Vec<String>,
    // Native DocDB hash group; an empty group is range sharded. None means no native sharding evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub hash_columns: Option<Vec<String>>,
}
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct PlanAutomaticIndex {
    pub table: String,
    pub columns: Vec<String>,
}
impl PlanAutomaticIndex {
    /// SQLite's `table(col, col)` description, also retained by older normalized plans.
    pub fn from_description(value: &str) -> Option<Self> {
        let (table, cols) = value.split_once('(')?;
        let columns = cols
            .trim_end_matches(')')
            .split(',')
            .filter_map(|c| {
                let name: String = c.trim().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                (!name.is_empty()).then_some(name)
            })
            .collect();
        Some(Self { table: table.trim().into(), columns })
    }
}
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanConversionIssue {
    SeekPlan,
    CardinalityEstimate,
    Other(String),
}
impl PlanConversionIssue {
    pub fn from_source(value: &str) -> Self {
        match value {
            "Seek Plan" => Self::SeekPlan,
            "Cardinality Estimate" => Self::CardinalityEstimate,
            v => Self::Other(v.into()),
        }
    }
    fn label(&self) -> &str {
        match self {
            Self::SeekPlan => "Seek Plan",
            Self::CardinalityEstimate => "Cardinality Estimate",
            Self::Other(v) => v,
        }
    }
}
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct PlanConversion {
    pub issue: PlanConversionIssue,
    pub expression: String,
}
impl PlanConversion {
    fn from_description(value: &str) -> Self {
        let (issue, expression) = value.split_once(": ").unwrap_or(("Convert", value));
        Self { issue: PlanConversionIssue::from_source(issue), expression: expression.into() }
    }
    pub fn text(&self) -> String {
        format!("{}: {}", self.issue.label(), self.expression)
    }
}
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct PlanChangedColumn {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    pub data_type: String,
}
fn legacy_indexes(value: &str) -> Vec<PlanIndexColumns> {
    value
        .lines()
        .filter_map(|line| {
            let (name, cols) = line.split_once('=')?;
            Some(PlanIndexColumns {
                hash_columns: None,
                name: name.into(),
                columns: cols.split(',').filter(|k| !k.is_empty()).map(str::to_string).collect(),
            })
        })
        .collect()
}
