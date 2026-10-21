use super::plan::is_measurement;
use super::*;

#[test]
fn a_redacted_snapshot_keeps_no_planted_value() {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut snapshot: Snapshot = serde_json::from_str(&text).expect("snapshot");
    snapshot.tables[0].definition = Some("CREATE TABLE example(value TEXT DEFAULT N'PLANTED-default')".into());
    let w = snapshot.workload.as_mut().expect("workload");
    for q in &mut w.queries {
        q.text = format!("{} AND x = N'PLANTED-text'", q.text);
        for p in &mut q.plans {
            p.compiled = Some("@p0 = N'PLANTED-compiled'".into());
        }
        if let Some(plan) = &mut q.plan {
            plan.root.predicate = Some("[c] = N'PLANTED-predicate'".into());
            plan.props.insert("Parameters".into(), "@p0 = N'PLANTED-props'".into());
            // Insights written from the raw plan, as a collector leaves them.
            crate::insights::annotate(plan);
            if let Some(i) = plan.insights.as_mut() {
                i.note = Some("seen N'PLANTED-insight'".into());
            }
        }
    }
    let deadlock: crate::deadlock::Deadlock = serde_json::from_value(serde_json::json!({
        "id": "d1", "at": null, "source": "test", "pattern": "cycle", "objects": [], "headline": "N'PLANTED-headline'",
        "explanation": "N'PLANTED-explanation'", "fixes": [], "resources": [],
        "processes": [{"id": "p1", "session": "51", "statement": "UPDATE t SET a = N'PLANTED-deadlock'",
            "procedure": null, "login": "someone@PLANTED-login.com", "host": "PLANTED-host", "application": null,
            "database": null, "isolation": null, "wait_mode": null, "victim": true}]
    }))
    .expect("deadlock");
    snapshot.deadlocks = Some(
        serde_json::from_value(serde_json::json!({
            "deadlocks": [deadlock], "source": "test", "since_hours": 24.0, "counted": 1.0
        }))
        .expect("deadlocks"),
    );
    let out = serde_json::to_string(&redact_snapshot(&snapshot)).expect("json");
    assert!(
        !out.contains("PLANTED"),
        "{}",
        &out[out.find("PLANTED").unwrap_or(0).saturating_sub(200)..][..400.min(out.len())]
    );
}

#[test]
fn prose_keeps_its_words_and_loses_the_values_it_quotes() {
    let summary = "Plan 7 isn't the fast one: it was compiled for `@p0 = N'9b9143e3-90c5-f111-bf5b-000d3aacaa10', @p1 = (42)` \
                   and filters on `[status] = N'active'`.";
    let out = redact_prose(summary, Engine::Mssql);
    assert!(out.starts_with("Plan 7 isn't the fast one"), "{out}");
    assert!(out.contains("compiled for `@p0 = N'?', …`"), "{out}");
    assert!(out.contains("`[status] = N'?'`"), "{out}");
    assert!(!out.contains("9b9143e3") && !out.contains("active") && !out.contains("42"), "{out}");
    // One parameter, or a list that still says something, stays as it is.
    assert_eq!(redact_prose("`@p0 = ?`", Engine::Mssql), "`@p0 = ?`");
    assert_eq!(redact_prose("`nvarchar(4000), @p1 = ?`", Engine::Mssql), "`nvarchar(4000), @p1 = ?`");
}

#[test]
fn a_value_holding_a_backtick_cannot_unmask_the_values_after_it() {
    let out = redact_prose("compiled for `@p0 = N'a`b', @p1 = N'secret', @p2 = 4242`. Then `x = 'y'`.", Engine::Mssql);
    assert!(!out.contains("secret") && !out.contains("4242") && !out.contains("a`b"), "{out}");
    // A literal that never closes hides everything after it.
    let out = redact_prose("compiled for `@p0 = 1, @p1 = N'secret and more", Engine::Mssql);
    assert!(!out.contains("secret") && !out.contains("more"), "{out}");
    // A doubled quote is part of its value.
    let out = redact_prose("`@p0 = N'it''s secret'` done", Engine::Mssql);
    assert!(!out.contains("secret") && out.ends_with("done"), "{out}");
    // GUIDs go wherever they are; apostrophes and words stay.
    let out = redact_prose("tenant 9b9143e3-90c5-f111-bf5b-000d3aacaa10 isn't the plan's problem", Engine::Mssql);
    assert_eq!(out, "tenant ? isn't the plan's problem");
    // MySQL escapes quotes with backslashes.
    let out = redact_prose(r"`status = 'it\'s `secret`'` done", Engine::Mysql);
    assert!(!out.contains("secret"), "{out}");
}

#[test]
fn sql_server_literals_go_names_and_markers_stay() {
    let r = redact_sql(
        "SELECT TOP (5) [id] FROM [shop].[orders] WHERE [email] = N'ann@example.com' AND total > 12.5 AND id = @P1 -- vip\n",
        Engine::Mssql,
    );
    assert_eq!(r, "SELECT TOP (?) [id] FROM [shop].[orders] WHERE [email] = N'?' AND total > ? AND id = @P1 -- …\n");
    assert_eq!(
        redact_sql(
            "[db].[shop].[customers].[email]=CONVERT_IMPLICIT(nvarchar(200),[@P1],0) AND [x]=(42)",
            Engine::Mssql
        ),
        "[db].[shop].[customers].[email]=CONVERT_IMPLICIT(nvarchar(200),[@P1],?) AND [x]=(?)"
    );
    assert_eq!(redact_sql("WHERE [it's] = 'it''s' AND b = 0x1F", Engine::Mssql), "WHERE [it's] = '?' AND b = ?");
    // A LIKE pattern keeps its wildcard shape, never its text.
    assert_eq!(
        redact_sql("email LIKE '%@example.com' AND name LIKE 'Al%' AND note LIKE '%x%' AND c = '%'", Engine::Mysql),
        "email LIKE '%?' AND name LIKE '?%' AND note LIKE '%?%' AND c = '%?'"
    );
    assert_eq!(redact_sql("WHERE n LIKE N'%smith'", Engine::Mssql), "WHERE n LIKE N'%?'");
    assert_eq!(
        redact_sql("CAST(a AS decimal(12,2)) /* customer 42 */", Engine::Mssql),
        "CAST(a AS decimal(12,2)) /* … */"
    );
}

#[test]
fn postgres_literals_go_names_and_markers_stay() {
    assert_eq!(
        redact_sql("SELECT \"Id\" FROM orders WHERE status = 'open'::text AND id = $1 AND n > 3", Engine::Postgres),
        "SELECT \"Id\" FROM orders WHERE status = '?'::text AND id = $1 AND n > ?"
    );
    assert_eq!(
        redact_sql("note = E'it\\'s' OR note = $$a 'b' c$$ OR x = $tag$q$tag$", Engine::Postgres),
        "note = E'?' OR note = '?' OR x = '?'"
    );
    assert_eq!(
        redact_sql("created_at > (now() - '7 days'::interval)", Engine::Postgres),
        "created_at > (now() - '?'::interval)"
    );
    assert_eq!(redact_sql("name::character varying(20)", Engine::Postgres), "name::character varying(20)");
}

#[test]
fn mysql_literals_go_names_and_markers_stay() {
    assert_eq!(
        redact_sql(
            "SELECT `id` FROM orders WHERE note = \"a \\\" b\" AND s = 'x' AND id = ? AND n = 7 # tail",
            Engine::Mysql
        ),
        "SELECT `id` FROM orders WHERE note = \"?\" AND s = '?' AND id = ? AND n = ? # …"
    );
    assert_eq!(redact_sql("(`shop`.`orders`.`customer_id` = 7)", Engine::Mysql), "(`shop`.`orders`.`customer_id` = ?)");
    assert_eq!(redact_sql("SELECT 1--2", Engine::Mysql), "SELECT ?--?");
    assert_eq!(redact_sql("SELECT /*+ NO_INDEX(o) */ 1", Engine::Mysql), "SELECT /*+ NO_INDEX(o) */ ?");
}

#[test]
fn database_names_drop_from_four_part_names() {
    assert_eq!(
        strip_database("[shop_prod].[dbo].[orders].[id]=[@P1] AND [dbo].[orders].[x]=1 AND [a]"),
        "[dbo].[orders].[id]=[@P1] AND [dbo].[orders].[x]=1 AND [a]"
    );
    assert_eq!(
        strip_database("CONVERT_IMPLICIT(nvarchar(200),[db].[s].[t].[c],0)"),
        "CONVERT_IMPLICIT(nvarchar(200),[s].[t].[c],0)"
    );
}

#[test]
fn identifiers_with_digits_survive() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql] {
        assert_eq!(redact_sql("SELECT t1.col2 FROM ix_2024 t1", engine), "SELECT t1.col2 FROM ix_2024 t1");
    }
}

#[test]
fn showplan_attributes_are_redacted_and_stay_valid_xml() {
    let xml = r#"<StmtSimple StatementText="SELECT a FROM t WHERE b = N&apos;x&apos; AND c = 5" StatementId="1"><ColumnReference Column="@p1" ParameterCompiledValue="N&apos;abc&apos;" /><ScalarOperator ScalarString="[t].[b]=(42)"/></StmtSimple>"#;
    let out = redact_native(xml, Engine::Mssql);
    assert!(out.contains(r#"StatementText="SELECT a FROM t WHERE b = N&apos;?&apos; AND c = ?""#), "{out}");
    assert!(out.contains(r#"ParameterCompiledValue="N&apos;?&apos;""#));
    assert!(out.contains(r#"ScalarString="[t].[b]=(?)""#));
    assert!(out.contains(r#"StatementId="1""#));
    let named = r#"<Object Database="[shop_prod]" Schema="[dbo]" Table="[orders]" /><ScalarOperator ScalarString="[shop_prod].[dbo].[orders].[id]=(1)"/>"#;
    let out = redact_native(named, Engine::Mssql);
    assert!(!out.contains("shop_prod"), "{out}");
    assert!(out.contains(r#"Database="[database]""#) && out.contains(r#"ScalarString="[dbo].[orders].[id]=(?)""#));
}

#[test]
fn text_plans_keep_their_figures() {
    let pg = "Index Scan using ix on orders o  (cost=0.42..8.44 rows=1 width=4)\n  Index Cond: (customer_id = 42)\n  Filter: (status = 'open'::text)\n  Rows Removed by Filter: 12\nPlanning Time: 0.2 ms";
    let out = redact_native(pg, Engine::Postgres);
    assert!(out.contains("(cost=0.42..8.44 rows=1 width=4)"));
    assert!(out.contains("Index Cond: (customer_id = ?)"));
    assert!(out.contains("Filter: (status = '?'::text)"));
    assert!(out.contains("Rows Removed by Filter: 12"));
    assert!(out.contains("Planning Time: 0.2 ms"));
    let my = "-> Filter: (o.customer_id = 7)  (cost=0.35 rows=1) (actual time=0.1..0.1 rows=1 loops=1)";
    assert_eq!(
        redact_native(my, Engine::Mysql),
        "-> Filter: (o.customer_id = ?)  (cost=0.35 rows=1) (actual time=0.1..0.1 rows=1 loops=1)"
    );
}

#[test]
fn json_plans_keep_measurements() {
    let raw = r#"{"query_block": {"cost_info": {"query_cost": "1.25"}, "table": {"table_name": "orders", "attached_condition": "(`o`.`status` = 'open')"}}}"#;
    let out = redact_native(raw, Engine::Mysql);
    assert!(out.contains("\"query_cost\": \"1.25\""));
    assert!(out.contains("(`o`.`status` = '?')"));
    assert!(out.contains("\"table_name\": \"orders\""));
}

// A query hint's number tunes the plan: the parallelism insight reads `OPTION (MAXDOP 1)` in a redacted statement
// as it does in the original.
#[test]
fn query_hints_keep_their_numbers() {
    assert_eq!(
        redact_sql("SELECT * FROM t WHERE id = 5 OPTION (MAXDOP 1, FAST 10)", Engine::Mssql),
        "SELECT * FROM t WHERE id = ? OPTION (MAXDOP 1, FAST 10)"
    );
    assert_eq!(redact_sql("SELECT TOP 5 x FROM t WHERE y > 3", Engine::Mssql), "SELECT TOP ? x FROM t WHERE y > ?");
}

// MySQL writes sizes with a unit ("106M" read per join): a measurement, not a literal to mask as "?M".
#[test]
fn a_number_with_a_unit_is_a_measurement() {
    let raw = r#"{"query_block": {"table": {"table_name": "orders", "cost_info": {"data_read_per_join": "106M", "read_cost": "12.50"}, "attached_condition": "(`o`.`code` = '106M')"}}}"#;
    let out = redact_native(raw, Engine::Mysql);
    assert!(out.contains("\"data_read_per_join\": \"106M\""), "{out}");
    assert!(out.contains("(`o`.`code` = '?')"), "{out}");
    for kept in ["106M", "3.5K", "1G", "8kB", "0.5 ms", "10%", "1.25"] {
        assert!(is_measurement(kept), "{kept}");
    }
    for masked in ["106 rows", "M106", "abc", "10M'", "'106M'"] {
        assert!(!is_measurement(masked), "{masked}");
    }
}

#[test]
fn endpoint_tokens_are_repeatable_and_never_merge_different_databases() {
    use crate::{DatabaseIdentity, DatabaseLocation};
    for location in [
        DatabaseLocation::Server {
            host: "private-endpoint.example".into(),
            port: 5432,
            instance: Some("private-instance".into()),
        },
        DatabaseLocation::Socket { directory: "/private/socket-directory".into(), port: 5432 },
        DatabaseLocation::File { path: "/Users/private-person/database.sqlite".into() },
        DatabaseLocation::Unknown,
    ] {
        let original = DatabaseIdentity {
            engine: Engine::Postgres,
            database: "catalog_label".into(),
            location,
            incarnation: Some("private-incarnation".into()),
        };
        let mut redacted = original.clone();
        redact_identity(&mut redacted);
        let once = redacted.clone();
        redact_identity(&mut redacted);
        assert_eq!(once, redacted);
        assert!(once.comparison(&redacted).allows_comparison());
        let encoded = serde_json::to_string(&redacted).unwrap();
        for secret in
            ["private-endpoint", "private-instance", "private-person", "socket-directory", "private-incarnation"]
        {
            assert!(!encoded.contains(secret));
        }
        let mut different = original;
        different.incarnation = Some("another-incarnation".into());
        redact_identity(&mut different);
        assert!(!redacted.comparison(&different).allows_comparison());
    }
    let mut invalid = DatabaseIdentity::server(Engine::Postgres, "label", "", 0, None);
    redact_identity(&mut invalid);
    assert!(!invalid.comparison(&invalid).allows_comparison());
}

#[test]
fn sharing_keeps_public_deadlock_limits_without_private_collector_details() {
    let mut snapshot: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/postgres-demo.json")).unwrap();
    for (engine, public_detail) in
        [(Engine::Postgres, "server log"), (Engine::Mysql, "most recent deadlock"), (Engine::Mssql, "details omitted")]
    {
        snapshot.server.engine = engine;
        snapshot.deadlocks = Some(crate::DeadlockReport {
            note: Some("Collection failed on private-collector-endpoint".into()),
            ..Default::default()
        });
        let shared = redact_snapshot(&snapshot);
        let note = shared.deadlocks.as_ref().unwrap().note.as_deref().unwrap();
        assert!(note.contains(public_detail));
        assert!(!note.contains("private-collector-endpoint"));
        assert_eq!(redact_snapshot(&shared).deadlocks.as_ref().unwrap().note.as_deref(), Some(note));
        snapshot.deadlocks.as_mut().unwrap().note = None;
        assert!(redact_snapshot(&snapshot).deadlocks.unwrap().note.is_none());
    }
}

#[test]
fn shared_snapshots_scrub_catalog_literals_and_all_retained_endpoint_references() {
    use crate::{BaselinePurpose, BaselineReference, CountBaseline, ScanReference};
    let mut snapshot: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/postgres-demo.json")).unwrap();
    snapshot.bind_identity(crate::DatabaseIdentity::server(
        Engine::Postgres,
        &snapshot.server.database.clone(),
        "private-endpoint-canary",
        5432,
        None,
    ));
    snapshot.ensure_measurement();
    let reference = Some(ScanReference::of(&snapshot));
    let measurement = snapshot.workload.as_ref().unwrap().measurement.clone();
    snapshot.workload.as_mut().unwrap().history_inputs.push(BaselineReference {
        purpose: BaselinePurpose::WorkloadTrend,
        scan: reference.clone().unwrap(),
        measurement: measurement.clone(),
    });
    snapshot.escalations_before =
        Some(crate::EscalationBaseline { reference: reference.clone(), ..Default::default() });
    snapshot.contention_before = Some(crate::ContentionBaseline { reference: reference.clone(), ..Default::default() });
    snapshot.evictions_before = Some(CountBaseline { reference: reference.clone(), measurement, ..Default::default() });
    snapshot.sizes_before = Some(crate::SizeBaseline { reference: reference.clone(), ..Default::default() });
    snapshot.operations.sqlite = Some(crate::operations::SqliteFileHealth {
        wal_bytes: Some(100.0),
        before: Some(crate::operations::CounterReading { at: snapshot.taken_at.clone(), count: 50.0, reference }),
    });
    snapshot.indexes[0].filter = Some("tenant = 'literal-value-canary'".into());
    snapshot.indexes[0].definition =
        Some("CREATE INDEX ix ON public.orders (id) WHERE tenant = 'literal-value-canary'".into());
    snapshot.columns[0].default = Some("'literal-value-canary'".into());
    snapshot.tuning = Some(crate::TuningAdvice {
        recommendations: vec![crate::TuningRecommendation {
            script: Some("SELECT 'literal-value-canary'".into()),
            reason: Some("Value 'literal-value-canary' was observed".into()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let shared = redact_snapshot(&snapshot);
    let output = serde_json::to_string(&shared).unwrap();
    assert!(!output.contains("private-endpoint-canary"));
    assert!(!output.contains("literal-value-canary"));
    assert_eq!(shared.server.database, snapshot.server.database, "database labels are intentional digest metadata");
    assert_eq!(shared.evictions_before.as_ref().unwrap().count, snapshot.evictions_before.as_ref().unwrap().count);
    assert_eq!(serde_json::to_value(&shared).unwrap(), serde_json::to_value(redact_snapshot(&shared)).unwrap());
}

#[test]
fn plan_facts_and_plugin_fix_sql_share_the_same_redaction_boundary() {
    let mut report: crate::Report =
        serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.report.json")).unwrap();
    let plan = report.snapshot.workload.as_mut().unwrap().queries.iter_mut().find_map(|q| q.plan.as_mut()).unwrap();
    let value = "[plan-database-canary].[dbo].[orders].[tenant] = 'literal-value-canary'";
    plan.props.insert("PlanAffectingConvert".into(), value.into());
    let facts = plan.root.facts.get_or_insert_default();
    facts.output_columns = Some(vec![value.into()]);
    facts.columns_without_statistics = vec![value.into()];
    facts.user_functions = vec!["[plan-database-canary].[dbo].[function_name]".into()];
    let redacted = serde_json::to_string(&redact_plan(plan)).unwrap();
    assert!(!redacted.contains("plan-database-canary"));
    assert!(!redacted.contains("literal-value-canary"));
    let finding = &mut report.findings[0];
    finding.title = "Value 'literal-value-canary'".into();
    finding.fix = Some(crate::Fix {
        summary: "Use the predicate".into(),
        sql: Some("CREATE INDEX ix ON dbo.t (x) WHERE tenant = 'literal-value-canary'".into()),
        cost: None,
    });
    assert!(!serde_json::to_string(&redact_finding(finding, Engine::Mssql)).unwrap().contains("literal-value-canary"));
}
