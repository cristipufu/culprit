use crate::{
    NodeKind, Plan, PlanExecutionMode, PlanFacts, PlanNode, PlanNodeFacts, Snapshot,
    plan::{MEASURED_KEPT, NOT_READ_IN_FULL, PARALLEL_THREADS},
};

fn plan() -> Plan {
    let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    let mut p = s.queries()[0].plan.as_ref().unwrap().clone();
    p.kind = crate::PlanKind::Actual;
    p.facts = None;
    p.props.clear();
    p.root = scan();
    p.root.act_rows = Some(10.0);
    p.root.act_executions = Some(1.0);
    p.root.act_time_ms = Some(20.0);
    p
}

#[test]
fn statement_facts_drive_actuals_and_memory_advice_without_display_properties() {
    let mut p = plan();
    p.props.insert("MemoryGrantedKB".into(), "1024000".into());
    p.props.insert("MemoryMaxUsedKB".into(), "1".into());
    p.props.insert("ElapsedMs".into(), "999".into());
    assert!(crate::insights::insights(&p).iter().any(|i| i.rule == "memory-grant"));
    p.facts = Some(PlanFacts { elapsed_ms: Some(0.0), memory_granted_kb: Some(1024000.0), ..Default::default() });
    let summary = crate::actual_summary(&p).unwrap();
    assert_eq!((summary.elapsed_ms, summary.memory_used_kb), (Some(0.0), None));
    assert!(!crate::insights::insights(&p).iter().any(|i| i.rule == "memory-grant"), "unknown use is not zero use");
    p.facts_mut().memory_max_used_kb = Some(0.0);
    assert!(crate::insights::insights(&p).iter().any(|i| i.rule == "memory-grant"));
    p.props.clear();
    let reread: Plan = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
    let summary = crate::actual_summary(&reread).unwrap();
    assert_eq!((summary.elapsed_ms, summary.memory_used_kb), (Some(0.0), Some(0.0)));
    assert!(crate::insights::insights(&reread).iter().any(|i| i.rule == "memory-grant"));
}

#[test]
fn invalid_imported_numbers_become_unknown_and_do_not_revive_legacy_properties() {
    let mut p = plan();
    p.props.insert("MemoryMaxUsedKB".into(), "0".into());
    let mut value = serde_json::to_value(&p).unwrap();
    value["facts"] = serde_json::json!({"memory_granted_kb": 1024000, "memory_max_used_kb": -1, "elapsed_ms": -1});
    value["root"]["facts"] = serde_json::json!({"measured_kept_share": 1.1, "estimated_filtered_percent": -1,
        "workers_planned": -1, "workers_launched": 0, "hash_batches": 1.5});
    let p: Plan = serde_json::from_value(value).unwrap();
    assert_eq!(p.facts().memory_max_used_kb, None);
    assert_eq!(p.facts().elapsed_ms, None);
    assert_eq!(p.root.facts().measured_kept_share, None);
    assert_eq!(p.root.facts().estimated_filtered_percent, None);
    assert_eq!(p.root.facts().workers_planned, None);
    assert_eq!(p.root.facts().workers_launched, Some(0));
    assert_eq!(p.root.facts().hash_batches, None);
    assert!(!crate::insights::insights(&p).iter().any(|i| i.rule == "memory-grant"));
}

#[test]
fn worker_advice_uses_typed_counts_and_explicit_unknowns_withdraw_stale_warnings() {
    let mut p = plan();
    p.engine = crate::Engine::Postgres;
    p.root.op = "Gather".into();
    p.root.props.insert("Workers Planned".into(), "4".into());
    p.root.props.insert("Workers Launched".into(), "4".into());
    p.root.facts = Some(PlanNodeFacts { workers_planned: Some(4), workers_launched: Some(0), ..Default::default() });
    let rules = |p: &Plan| crate::insights::insights(p).into_iter().map(|i| i.rule).collect::<Vec<_>>();
    assert!(rules(&p).contains(&"workers-short".into()));
    p.root.facts = None;
    assert!(!rules(&p).contains(&"workers-short".into()));
    p.root.props.insert("Workers Launched".into(), "0".into());
    assert!(rules(&p).contains(&"workers-short".into()));
    p.root.facts = Some(PlanNodeFacts::default());
    assert!(!rules(&p).contains(&"workers-short".into()));
}

#[test]
fn parallel_skew_uses_real_workers_and_older_bounds_do_not_allocate_a_worker_vector() {
    let mut n = scan();
    n.act_rows = Some(100000.0);
    n.props.insert("ThreadCount".into(), "4294967295".into());
    n.props.insert("ThreadRowsMax".into(), "90000".into());
    assert_eq!(
        crate::plan_facts::parallel_distribution(&n, crate::Engine::Mssql),
        Some((4294967295, 100000.0, 90000.0))
    );
    n.facts = Some(PlanNodeFacts::default());
    assert_eq!(crate::plan_facts::parallel_distribution(&n, crate::Engine::Mssql), None);
    n.runtime = Some(crate::NodeRuntime { thread_rows: vec![90000.0, 3000.0, 2000.0], ..Default::default() });
    assert_eq!(crate::plan_facts::parallel_distribution(&n, crate::Engine::Mssql), Some((3, 95000.0, 90000.0)));
    for engine in [crate::Engine::Mysql, crate::Engine::Sqlite] {
        assert_eq!(crate::plan_facts::parallel_distribution(&n, engine), None);
    }
    n.runtime.as_mut().unwrap().thread_rows[1] = -1.0;
    assert_eq!(crate::plan_facts::parallel_distribution(&n, crate::Engine::Mssql), None);
}

#[test]
fn spill_sizes_keep_native_precision_and_do_not_use_stale_property_fallbacks() {
    let mut n = scan();
    n.props.insert("TempdbWritesPages".into(), "8".into());
    assert_eq!(crate::plan_facts::spill_pages(&n), Some(8.0));
    n.facts = Some(PlanNodeFacts::default());
    assert_eq!(crate::plan_facts::spill_pages(&n), None);
    n.runtime = Some(crate::NodeRuntime { spill_pages: Some(0.0), ..Default::default() });
    assert_eq!(crate::plan_facts::spill_pages(&n), Some(0.0));
    n.runtime.as_mut().unwrap().spill_pages = Some(19.0);
    assert_eq!(crate::plan_facts::spill_pages(&n), Some(19.0));
}

#[test]
fn index_methods_require_one_catalog_identity_and_stale_marks_are_withdrawn() {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
    let mut index = s.indexes[0].clone();
    index.schema = "public".into();
    index.table = "events".into();
    index.name = "same_name".into();
    index.kind = "brin".into();
    let mut other = index.clone();
    other.schema = "archive".into();
    other.kind = "btree".into();
    s.indexes = vec![index, other];
    let mut root = scan();
    root.op = "Bitmap Heap Scan".into();
    root.object = Some("public.events".into());
    let mut child = scan();
    child.op = "Bitmap Index Scan".into();
    child.kind = NodeKind::Seek;
    child.object = None;
    child.index = Some("same_name".into());
    root.children = vec![child];
    s.workload.as_mut().unwrap().queries[0].plan.as_mut().unwrap().root = root;
    crate::insights::mark_snapshot_index_kinds(&mut s);
    assert!(s.queries()[0].plan.as_ref().unwrap().root.children[0].reads_brin());
    s.workload.as_mut().unwrap().queries[0].plan.as_mut().unwrap().root.object = None;
    crate::insights::mark_snapshot_index_kinds(&mut s);
    let child = &s.queries()[0].plan.as_ref().unwrap().root.children[0];
    assert_eq!(child.facts().index_method, None);
    assert!(!child.props.contains_key(crate::plan::INDEX_KIND));
    s.indexes.clear();
    crate::insights::mark_snapshot_index_kinds(&mut s);
    assert_eq!(s.queries()[0].plan.as_ref().unwrap().root.children[0].facts().index_method, None);
}

#[test]
fn candidate_index_columns_are_qualified_and_refresh_removes_obsolete_evidence() {
    let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mysql-demo.json")).unwrap();
    let mut index = s.indexes[0].clone();
    index.schema = "shop".into();
    index.table = "orders".into();
    index.name = "same_name".into();
    index.columns = vec!["wanted".into()];
    let mut other = index.clone();
    other.schema = "archive".into();
    other.columns = vec!["unrelated".into()];
    let mut p = plan();
    p.engine = crate::Engine::Mysql;
    p.root.object = Some("shop.orders".into());
    p.root.facts = Some(PlanNodeFacts { possible_index_names: vec!["same_name".into()], ..Default::default() });
    crate::insights::mark_possible_keys(&mut p, &[index.clone(), other.clone()]);
    assert_eq!(p.root.facts().possible_index_columns.as_ref().unwrap()[0].columns, vec!["wanted"]);
    p.root.object = Some("orders".into());
    crate::insights::mark_possible_keys(&mut p, &[index, other]);
    assert_eq!(p.root.facts().possible_index_columns, None);
    assert!(!p.root.props.contains_key("possible_keys_columns"));
    crate::insights::mark_possible_keys(&mut p, &[]);
    assert_eq!(p.root.facts().possible_index_columns, None);
}

#[test]
fn changed_columns_keep_schema_and_target_type_and_can_be_withdrawn() {
    let mut p = plan();
    let expr = |schema| format!("CONVERT_IMPLICIT(nvarchar(64),[db].[{schema}].[jobs].[name],0)=[@p1]");
    let a = expr("current");
    let b = expr("archive");
    p.facts = Some(PlanFacts {
        conversions: vec![crate::PlanConversion {
            issue: crate::PlanConversionIssue::SeekPlan,
            expression: format!("{a} AND {b}"),
        }],
        ..Default::default()
    });
    let column = |schema: &str, ty: &str| crate::ColumnInfo {
        schema: schema.into(),
        table: "jobs".into(),
        name: "name".into(),
        data_type: ty.into(),
        nullable: false,
        default: None,
        generated: false,
        ordinal: 1,
    };
    crate::insights::mark_changed_columns(
        &mut p,
        &[column("current", "nvarchar(64)"), column("archive", "varchar(64)")],
    );
    assert!(crate::insights::current_conversions(&p, &a).is_empty());
    assert_eq!(crate::insights::current_conversions(&p, &b).len(), 1);
    let another_target = "CONVERT_IMPLICIT(varbinary(64),[db].[current].[jobs].[name],0)=[@p1]";
    assert_eq!(crate::insights::current_conversions(&p, another_target).len(), 1);
    crate::insights::mark_changed_columns(&mut p, &[]);
    assert_eq!(crate::insights::current_conversions(&p, &a).len(), 1);
    assert!(!p.props.contains_key(crate::insights::CHANGED_SINCE_CAPTURE));
}

#[test]
fn conversion_context_resolves_native_aliases_and_refuses_aliases_shared_by_different_tables() {
    let mut p = plan();
    p.root.object = Some("current.jobs".into());
    p.root.facts = Some(PlanNodeFacts { table_alias: Some("j".into()), ..Default::default() });
    let aliased = "CONVERT_IMPLICIT(nvarchar(64),[j].[name],0)=[@p1]";
    let physical = "CONVERT_IMPLICIT(nvarchar(64),[db].[current].[jobs].[name] as [j].[name],0)=[@p1]";
    p.root.predicate = Some(physical.into());
    p.facts = Some(PlanFacts {
        conversions: vec![crate::PlanConversion {
            issue: crate::PlanConversionIssue::SeekPlan,
            expression: aliased.into(),
        }],
        ..Default::default()
    });
    let column = crate::ColumnInfo {
        schema: "current".into(),
        table: "jobs".into(),
        name: "name".into(),
        data_type: "nvarchar(64)".into(),
        nullable: false,
        default: None,
        generated: false,
        ordinal: 1,
    };
    crate::insights::mark_changed_columns(&mut p, std::slice::from_ref(&column));
    assert!(crate::insights::current_conversions(&p, aliased).is_empty());
    assert!(crate::insights::current_conversions(&p, physical).is_empty());
    let mut other = p.root.clone();
    other.object = Some("archive.jobs".into());
    other.children.clear();
    p.root.children = vec![other];
    crate::insights::mark_changed_columns(&mut p, &[column]);
    assert_eq!(crate::insights::current_conversions(&p, aliased).len(), 1);
    assert!(crate::insights::current_conversions(&p, physical).is_empty());
    p.root.children[0].object = None;
    crate::insights::mark_changed_columns(&mut p, &[]);
    assert_eq!(crate::insights::current_conversions(&p, physical)[0].0, "jobs.name");
    let quoted_as = "CONVERT_IMPLICIT(nvarchar(64),[db].[current].[jobs as old].[name],0)=[@p1]";
    assert_eq!(crate::insights::current_conversions(&p, quoted_as)[0].0, "jobs as old.name");
    let column = crate::ColumnInfo {
        schema: "current".into(),
        table: "jobs".into(),
        name: "name".into(),
        data_type: "nvarchar(64)".into(),
        nullable: false,
        default: None,
        generated: false,
        ordinal: 1,
    };
    crate::insights::mark_changed_columns(&mut p, &[column]);
    assert_eq!(crate::insights::current_conversions(&p, aliased).len(), 1);
}

#[test]
fn typed_fact_redaction_masks_values_and_preserves_measurements_and_original_evidence() {
    let mut p = plan();
    p.facts = Some(PlanFacts {
        elapsed_ms: Some(0.004),
        compiled_parameters: Some("@p1 = N'PLANTED_PARAMETER'".into()),
        conversions: vec![crate::PlanConversion {
            issue: crate::PlanConversionIssue::SeekPlan,
            expression: "CONVERT_IMPLICIT(nvarchar(64),[db].[shop].[t].[c],0)=N'PLANTED_CONVERT'".into(),
        }],
        generic_plan_reason: Some("Planned with `x = 123` and 'PLANTED_GENERIC'".into()),
        ..Default::default()
    });
    p.root.facts = Some(PlanNodeFacts {
        conditions: vec!["value = N'PLANTED_CONDITION'".into()],
        join_filter: Some("value = N'PLANTED_JOIN'".into()),
        selectivity_guess: Some("For 'PLANTED_GUESS'".into()),
        ..Default::default()
    });
    let redacted = crate::redact::redact_plan(&p);
    assert!(!serde_json::to_string(&redacted).unwrap().contains("PLANTED_"));
    assert_eq!(redacted.facts().elapsed_ms, Some(0.004));
    assert!(p.facts().compiled_parameters.as_ref().unwrap().contains("PLANTED_PARAMETER"));
}

#[test]
fn redacting_plan_expressions_preserves_subplan_references_but_masks_lookalike_values() {
    let mut p = plan();
    p.engine = crate::Engine::Postgres;
    p.root.predicate = Some("value = 123 AND (SubPlan 12) AND label = 'SubPlan 456'".into());
    p.root.facts = Some(PlanNodeFacts { conditions: vec![p.root.predicate.clone().unwrap()], ..Default::default() });
    let redacted = crate::redact::redact_plan(&p);
    let predicate = redacted.root.predicate.as_ref().unwrap();
    assert!(predicate.contains("(SubPlan 12)"));
    assert!(!predicate.contains("123") && !predicate.contains("456"));
    assert!(redacted.root.facts().conditions[0].contains("(SubPlan 12)"));
}

#[test]
fn row_width_uses_typed_evidence_without_reviving_stale_display_numbers() {
    let mut p = plan();
    p.root.props.insert("AvgRowSize".into(), "8000".into());
    assert_eq!(crate::signals::plan_row_bytes(&p), Some(8000.0));
    p.root.facts = Some(PlanNodeFacts::default());
    assert_eq!(crate::signals::plan_row_bytes(&p), None);
    p.root.facts_mut().estimated_row_bytes = Some(64.0);
    assert_eq!(crate::signals::plan_row_bytes(&p), Some(64.0));
}

#[test]
fn refreshing_parallel_settings_withdraws_stale_context_and_keeps_valid_zero() {
    use crate::insights::{COST_THRESHOLD, DATABASE_MAXDOP, SERVER_MAXDOP, mark_parallel_settings};
    let mut p = plan();
    p.props.insert(SERVER_MAXDOP.into(), "1".into());
    mark_parallel_settings(&mut p, &[(DATABASE_MAXDOP, "0".into()), (COST_THRESHOLD, "50".into())]);
    assert_eq!((p.facts().server_maxdop, p.facts().database_maxdop), (None, Some(0)));
    assert!(!p.props.contains_key(SERVER_MAXDOP));
    mark_parallel_settings(&mut p, &[(SERVER_MAXDOP, "-1".into()), (COST_THRESHOLD, "NaN".into())]);
    assert_eq!(
        (p.facts().cost_threshold_for_parallelism, p.facts().server_maxdop, p.facts().database_maxdop),
        (None, None, None)
    );
    assert!(!p.props.contains_key(DATABASE_MAXDOP));
}

#[test]
fn trigger_measurements_reject_invalid_time_and_invalid_call_counts() {
    use crate::actual::TriggerTiming;
    let mut p = plan();
    p.engine = crate::Engine::Postgres;
    p.facts = Some(PlanFacts {
        trigger_timings: vec![
            TriggerTiming { name: "audit".into(), relation: None, time_ms: 0.004, calls: Some(1.0) },
            TriggerTiming { name: "missing calls".into(), relation: None, time_ms: 0.0, calls: Some(-1.0) },
            TriggerTiming { name: "invalid calls".into(), relation: None, time_ms: 1.0, calls: Some(1.5) },
            TriggerTiming { name: "invalid time".into(), relation: None, time_ms: -1.0, calls: Some(1.0) },
        ],
        ..Default::default()
    });
    let triggers = crate::actual::trigger_timings(&p);
    assert_eq!(triggers.len(), 3);
    assert_eq!((triggers[0].time_ms, triggers[0].calls), (0.004, Some(1.0)));
    assert_eq!(triggers[1].calls, None);
    assert_eq!(triggers[2].calls, None);
    let imported: PlanFacts = serde_json::from_value(serde_json::json!({"trigger_timings":[
        {"name":"missing time"}, {"name":"invalid time","time_ms":-1},
        {"name":"zero","time_ms":0,"calls":1.5}, {"name":"measured","time_ms":0.004,"calls":1}
    ]}))
    .unwrap();
    assert_eq!(imported.trigger_timings.len(), 2);
    assert_eq!((imported.trigger_timings[0].time_ms, imported.trigger_timings[0].calls), (0.0, None));
    assert_eq!((imported.trigger_timings[1].time_ms, imported.trigger_timings[1].calls), (0.004, Some(1.0)));
}

fn scan() -> PlanNode {
    let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    let mut n = s.queries()[0].plan.as_ref().unwrap().root.clone();
    n.kind = NodeKind::Scan;
    n.op = "Table Scan".into();
    n.object = Some("shop.orders".into());
    n.act_rows = None;
    n.act_executions = None;
    n.act_time_ms = None;
    n.runtime = None;
    n.est_rows = 10.0;
    n.est_executions = 1.0;
    n.rows_read = Some(100.0);
    n.table_rows = Some(1000.0);
    n.children.clear();
    n.props.clear();
    n.facts = None;
    n
}

#[test]
fn older_plans_read_properties_but_present_facts_never_inherit_stale_display_values() {
    let mut n = scan();
    n.props.insert(MEASURED_KEPT.into(), "0.004".into());
    assert_eq!(n.kept_share(), Some(0.004));
    let saved = serde_json::to_value(&n).unwrap();
    assert!(saved.get("facts").is_none());
    let older: PlanNode = serde_json::from_value(saved).unwrap();
    assert_eq!(older.kept_share(), Some(0.004));
    n.facts = Some(PlanNodeFacts::default());
    assert_eq!(n.kept_share(), Some(0.1), "the original row counts remain evidence; the stale property does not");
    let reread: PlanNode = serde_json::from_value(serde_json::to_value(&n).unwrap()).unwrap();
    assert!(reread.facts.is_some());
    assert_eq!(reread.kept_share(), Some(0.1));
}

#[test]
fn tiny_observed_selectivity_keeps_its_precision_and_a_real_zero_remains_zero() {
    let mut n = scan();
    let exact = 1.0 / 9_000_000_000.0;
    n.facts = Some(PlanNodeFacts { measured_kept_share: Some(exact), ..Default::default() });
    n.props.insert(MEASURED_KEPT.into(), "0.000000".into());
    assert_eq!(n.kept_share(), Some(exact));
    let reread: PlanNode = serde_json::from_value(serde_json::to_value(&n).unwrap()).unwrap();
    assert_eq!(reread.kept_share(), Some(exact));
    n.facts_mut().measured_kept_share = Some(0.0);
    assert_eq!(n.kept_share(), Some(0.0));
}

#[test]
fn invalid_selectivity_is_unknown_rather_than_an_empty_result() {
    for value in ["NaN", "inf", "-0.01", "1.1"] {
        let mut n = scan();
        n.rows_read = None;
        n.table_rows = None;
        n.props.insert(MEASURED_KEPT.into(), value.into());
        assert_eq!(n.kept_share(), None, "{value}");
        n.facts_mut().measured_kept_share = value.parse().ok();
        assert_eq!(n.kept_share(), None, "typed {value}");
    }
    let mut n = scan();
    n.props.insert(MEASURED_KEPT.into(), "0".into());
    assert_eq!(n.kept_share(), Some(0.0));
}

#[test]
fn runtime_threads_normalize_executions_independently_of_properties() {
    let mut n = scan();
    n.act_executions = Some(4.0);
    n.props.insert(PARALLEL_THREADS.into(), "4".into());
    assert_eq!(n.runs(), 1.0);
    n.facts = Some(PlanNodeFacts { parallel_threads: Some(2), ..Default::default() });
    assert_eq!(n.runs(), 3.0);
    n.props.clear();
    assert_eq!(n.runs(), 3.0);
    n.act_executions = Some(0.0);
    assert_eq!(n.runs(), 0.0, "an operator that never ran is not one execution");
}

#[test]
fn a_prunable_partition_cannot_borrow_its_full_catalog_size_as_rows_read() {
    let mut n = scan();
    n.rows_read = None;
    n.facts = Some(PlanNodeFacts {
        partition_pruning: Some("Pruned when parameter values are known".into()),
        ..Default::default()
    });
    assert_eq!(n.rows_read_total(), None);
    assert!(!n.reads_table_whole());
    n.props.clear();
    assert_eq!(n.rows_read_total(), None);
    n.facts = None;
    n.props.insert(NOT_READ_IN_FULL.into(), "An older captured pruning mark".into());
    assert_eq!(n.rows_read_total(), None);
    assert!(!n.reads_table_whole());
}

#[test]
fn execution_mode_facts_control_own_time_without_reading_display_labels() {
    let mut root = scan();
    root.id = 1;
    root.act_rows = Some(10.0);
    root.act_executions = Some(1.0);
    root.act_time_ms = Some(10.0);
    let mut child = scan();
    child.id = 2;
    child.act_rows = Some(10.0);
    child.act_executions = Some(1.0);
    child.act_time_ms = Some(6.0);
    root.children = vec![child];
    root.facts = Some(PlanNodeFacts { actual_execution_mode: Some(PlanExecutionMode::Batch), ..Default::default() });
    root.props.insert("ActualExecutionMode".into(), "Row".into());
    let s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    let mut p = s.queries()[0].plan.as_ref().unwrap().clone();
    p.kind = crate::PlanKind::Actual;
    p.root = root;
    assert_eq!(crate::actual::self_times(&p).unwrap()[&1], 10.0);
    p.root.facts_mut().actual_execution_mode = Some(PlanExecutionMode::Row);
    p.root.facts_mut().estimated_execution_mode = Some(PlanExecutionMode::Batch);
    p.root.props.insert("ActualExecutionMode".into(), "Batch".into());
    assert_eq!(crate::actual::self_times(&p).unwrap()[&1], 4.0);
}
