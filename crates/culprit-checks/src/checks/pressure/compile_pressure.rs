//! The `compile-pressure` check: A large share of batches compile a new plan instead of reusing one, which costs CPU on every call.

use super::*;

pub fn compile_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "compile-pressure".into(),
        title: "Plans compiled over and over".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description:
            "A large share of batches compile a new plan instead of reusing one, which costs CPU on every call.".into(),
    }
}

check!(CompilePressureCheck: compile_pressure_info, compile_pressure);

pub fn compile_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(compiles) = pressure(snapshot).and_then(|p| p.signal("compiles-per-batch")) else { return Vec::new() };
    // An ungraded ratio is one the collector won't judge: on Azure SQL Database the counters are the server's behind
    // the database (its elastic pool's too), and a handful of batches says nothing about plan reuse.
    if compiles.status == SignalStatus::Info {
        return Vec::new();
    }
    let ratio = compiles.value;
    // The load behind the ratio, averaged over at least an hour since the start.
    let (Some(batches), Some(_)) = (signal(snapshot, "batch-requests"), counter_hours(snapshot)) else {
        return Vec::new();
    };
    if ratio < 15.0 || batches < 5.0 {
        return Vec::new();
    }
    let per_min = batches * ratio / 100.0 * 60.0;
    let mut f = finding(
        "compile-pressure",
        &server_key(snapshot),
        Category::Configuration,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    f.title = format!(
        "SQL Server compiles a new plan for {:.0}% of batches (about {} a minute)",
        ratio,
        format::count(per_min)
    );
    f.summary = format!(
        "{}, the instance averaged {}/s batches across all its databases and compiled a new plan for {:.0}% of them, \
         recompiles not counted. Healthy systems reuse plans and compile for under 10%. Compiling costs CPU on every \
         call; this usually means the application sends statements with literal values instead of parameters.",
        capitalize(&uptime_phrase(snapshot)),
        format::count(batches),
        ratio
    );
    // The counters are the instance's: this database is a likely source only when its own statements show the pattern
    // (families of one statement with different literal values). Otherwise another database on the instance compiles.
    let here = !snapshot.query_families.is_empty();
    if !here {
        f.summary.push_str(
            " This database's own statements don't show it (no families of one statement sent with different literal values), so another database on the instance is the likelier source.",
        );
    }
    // An average since the start describes the past: at most Medium.
    f.severity = if ratio >= 25.0 && here { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + ratio / 5.0;
    f.impact.label = format!("{:.0}% of batches compile", ratio);
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Compiles per 100 batches", ratio, Unit::Count),
        Metric::new("Compiles per minute", per_min, Unit::Count),
    ];
    if let Some(recompiles) = signal(snapshot, "recompiles-per-batch").filter(|r| *r > 0.0) {
        f.metrics.push(Metric::new("Recompiles per 100 batches", recompiles, Unit::Count));
    }
    // What cuts compiles is a plan that serves every value; caching one-off plans as stubs saves memory but compiles
    // just as often, so it isn't the fix here (the plan-cache finding offers it for the memory).
    f.fix = Some(if here {
        Fix {
            summary: "Send parameters instead of literal values (parameterized queries in the driver or ORM), so one plan serves every value. If you can't change the application, forced parameterization does it for the whole database.".into(),
            sql: Some(format!(
                "-- Prefer fixing the app. As a database-wide fallback:\nALTER DATABASE {} SET PARAMETERIZATION FORCED;",
                quote(Engine::Mssql, &snapshot.server.database)
            )),
            cost: Some("Forced parameterization changes plans for every query, and some may suit one value badly: test it first and watch the regression findings.".into()),
        }
    } else {
        Fix {
            summary: "Find the database whose application sends literal values instead of parameters: scanning each database on the instance names its families of one statement with different values (the query-family check).".into(),
            sql: None,
            cost: None,
        }
    });
    f.verify =
        vec!["Sample Right now in Waits (or re-scan): compilations should fall under 10% of batch requests.".into()];
    vec![f]
}
