//! Settings that decide whether Culprit can show what a query actually did: SQL Server's last actual plan per query and
//! live per-operator progress.

use crate::evidence::Requirement as Evidence;
use culprit_core::{Category, CheckInfo, Confidence, Engine, Finding, Fix, Severity, Snapshot, SubjectKind};

use culprit_core::Check;

use crate::support::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[&LastActualPlansCheck];

pub fn last_actual_plans_info() -> CheckInfo {
    CheckInfo {
        id: "last-actual-plans".into(),
        title: "Actual plans without re-running queries".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "SQL Server 2019+ can keep each query's last actual plan (LAST_QUERY_PLAN_STATS) and report live \
                      per-operator progress (LIGHTWEIGHT_QUERY_PROFILING)."
            .into(),
    }
}

check!(LastActualPlansCheck: last_actual_plans_info, last_actual_plans, &[Evidence::Unknown]);

fn off(snapshot: &Snapshot, name: &str) -> bool {
    snapshot.setting(name).is_some_and(|s| matches!(s.value.trim(), "0" | "OFF" | "off" | "false"))
}

/// Only reported when the server has the setting (2019+ and Azure SQL list it in sys.database_scoped_configurations)
/// and there is a workload worth looking at.
pub fn last_actual_plans(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Mssql || snapshot.workload.as_ref().is_none_or(|w| w.queries.is_empty()) {
        return Vec::new();
    }
    let mut out = Vec::new();
    if off(snapshot, "LAST_QUERY_PLAN_STATS") {
        let mut f = finding(
            "last-actual-plans",
            "last-query-plan-stats",
            Category::Configuration,
            subject(SubjectKind::Setting, "LAST_QUERY_PLAN_STATS"),
        );
        f.severity = Severity::Low;
        f.confidence = Confidence::High;
        f.impact.score = base_score(Severity::Low) * 0.6;
        f.impact.label = "Diagnostics".into();
        // Trace flag 2451 keeps them server-wide whatever this setting says; the scan reads no trace flags. Only a
        // server you manage takes it: Azure SQL Database has no trace flags, and the other managed services allow only
        // their own list, so there the sentence would point at something that can't be set.
        let trace_flag = if snapshot.server.is_managed() {
            ""
        } else {
            " (Trace flag 2451 turns the same on for the whole server; if it is set, actual plans are kept already.)"
        };
        f.title = "LAST_QUERY_PLAN_STATS is off for this database".into();
        f.summary = format!(
            "With LAST_QUERY_PLAN_STATS on, SQL Server can keep the real row counts and timings of an eligible cached \
             query's most recent run. Complex or resource-intensive queries can then show where they actually spent \
             their time, operator by operator, without running them again. Simple transactional queries may retain only the \
             root operator; plans that aren't cached or have been evicted retain no last-run data. Without retained \
             runtime data, the saved estimated plan cannot confirm a row misestimate.{trace_flag}"
        );
        f.fix = Some(Fix {
            summary: "Turn it on for this database.".into(),
            sql: Some("ALTER DATABASE SCOPED CONFIGURATION SET LAST_QUERY_PLAN_STATS = ON;".into()),
            cost: Some(
                "Uses SQL Server's lightweight profiling to keep a little runtime data per cached plan: a small CPU \
                 overhead on very busy servers."
                    .into(),
            ),
        });
        f.verify = vec![
            "Let the workload run, re-scan, then open a complex or resource-intensive query in Queries and pick \
             \"Last actual run\" when available: the diagram shows the rows and timings SQL Server retained. A simple \
             query may have only a root operator, and an uncached or evicted plan has no last-run data."
                .into(),
        ];
        out.push(f);
    }
    if off(snapshot, "LIGHTWEIGHT_QUERY_PROFILING") {
        let mut f = finding(
            "last-actual-plans",
            "lightweight-query-profiling",
            Category::Configuration,
            subject(SubjectKind::Setting, "LIGHTWEIGHT_QUERY_PROFILING"),
        );
        f.severity = Severity::Low;
        f.confidence = Confidence::High;
        f.impact.score = base_score(Severity::Low) * 0.6;
        f.impact.label = "Diagnostics".into();
        f.title = "Live progress of running queries is turned off".into();
        f.summary = "LIGHTWEIGHT_QUERY_PROFILING is off for this database, so SQL Server doesn't count rows per \
                     operator while a query runs. Live can show how long a statement has been running, but not how \
                     far along each step is or which step is slow right now."
            .into();
        f.fix = Some(Fix {
            summary: "Turn it back on (it is on by default since SQL Server 2019).".into(),
            sql: Some("ALTER DATABASE SCOPED CONFIGURATION SET LIGHTWEIGHT_QUERY_PROFILING = ON;".into()),
            cost: Some("SQL Server's default; designed to run all the time.".into()),
        });
        f.verify = vec!["Open Live while a long query runs: its row expands into per-operator progress.".into()];
        out.push(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str) -> Snapshot {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/{name}"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn explains_what_last_query_plan_stats_unlocks() {
        let s = snapshot("mssql-demo.json");
        let found = last_actual_plans(&s);
        assert_eq!(found.len(), 1, "the demo has LAST_QUERY_PLAN_STATS off and lightweight profiling on");
        assert_eq!(found[0].id, "last-actual-plans:last-query-plan-stats");
        assert!(found[0].fix.as_ref().unwrap().sql.as_deref().unwrap().contains("LAST_QUERY_PLAN_STATS = ON"));
        assert!(found[0].summary.contains("Trace flag 2451"), "a server you manage can set it: {}", found[0].summary);
        assert!(found[0].summary.contains("eligible cached"), "{}", found[0].summary);
        assert!(
            found[0].summary.contains("Simple transactional queries may retain only the root operator"),
            "{}",
            found[0].summary
        );
        assert!(found[0].verify[0].contains("when available"), "{:?}", found[0].verify);
        assert!(found[0].verify[0].contains("uncached or evicted"), "{:?}", found[0].verify);
        assert!(last_actual_plans(&snapshot("postgres-demo.json")).is_empty());
    }

    // Azure SQL Database has no trace flags: the finding doesn't point at one.
    #[test]
    fn a_trace_flag_is_mentioned_only_where_one_can_be_set() {
        let mut s = snapshot("mssql-demo.json");
        s.server.platform = Some(culprit_core::Platform::AzureSqlDatabase);
        let found = last_actual_plans(&s);
        assert!(!found[0].summary.contains("Trace flag"), "{}", found[0].summary);
        assert!(found[0].fix.as_ref().unwrap().sql.as_deref().unwrap().contains("LAST_QUERY_PLAN_STATS = ON"));
    }
}
