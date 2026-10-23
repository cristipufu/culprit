//! Offline report profiling against a repeatable workload. See docs/ARCHITECTURE.md for commands.
use std::{path::Path, time::Instant};

use culprit_core::{CollectorNote, Report, ReportEvidence};
use culprit_engine::{Workspace, reports};

fn prepare(dir: &Path, count: usize) -> Result<(), Box<dyn std::error::Error>> {
    let mut report: Report = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.report.json"))?;
    report.snapshot.connection = "profile".into();
    let workload = report.snapshot.workload.as_mut().ok_or("fixture has no workload")?;
    let query = workload.queries.iter().find(|q| q.plan.is_some()).ok_or("fixture has no plan")?.clone();
    workload.queries = (0..count)
        .map(|i| {
            let mut q = query.clone();
            q.id = format!("profile-{i}");
            q
        })
        .collect();
    workload.query_count = count as f64;
    workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
    workload.total_time_ms = workload.queries.iter().map(|q| q.total_time_ms).sum();
    let mut input = report.snapshot.clone();
    input.notes.push(CollectorNote { collector: "profile".into(), message: "Retained input".into() });
    let mut collected = input.clone();
    collected.notes.push(CollectorNote { collector: "profile".into(), message: "Original collection".into() });
    report.evidence = Some(ReportEvidence {
        input: Box::new(input),
        collected: Some(Box::new(collected)),
        recorded_changes: Vec::new(),
    });
    let workspace = Workspace::open_in(dir);
    let reports_dir = workspace.paths().reports_dir("profile");
    reports::save(&reports_dir, &report)?;
    println!("{}", reports_dir.join(format!("{}.json", report.snapshot.id)).display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args
        .first()
        .ok_or("usage: profile_reports prepare|load|snapshot|replay <isolated-directory> [queries|iterations]")?;
    let dir = Path::new(args.get(1).ok_or("an isolated directory is required")?);
    let amount: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(if mode == "prepare" { 1000 } else { 3 });
    if mode == "prepare" {
        return prepare(dir, amount.clamp(1, 5000));
    }
    if !matches!(mode.as_str(), "load" | "snapshot" | "replay") {
        return Err("unknown profiling mode".into());
    }
    let workspace = Workspace::open_in(dir);
    let reports_dir = workspace.paths().reports_dir("profile");
    let summary = reports::latest_summary(&reports_dir, None).ok_or("run prepare first")?;
    let path = reports_dir.join(format!("{}.json", summary.id));
    let iterations = amount.clamp(1, 20);
    let started = Instant::now();
    let mut queries = 0;
    for _ in 0..iterations {
        match mode.as_str() {
            "load" => {
                let report = reports::load(&path)?;
                queries = report.snapshot.queries().len();
                std::hint::black_box(report);
            }
            "snapshot" => {
                let snapshot = workspace.latest_snapshot("profile")?.ok_or("no snapshot")?;
                queries = snapshot.queries().len();
                std::hint::black_box(snapshot);
            }
            "replay" => {
                let report = workspace.analyze_file(&path)?;
                queries = report.snapshot.queries().len();
                assert!(report.analysis_input().notes.iter().any(|n| n.message == "Retained input"));
                std::hint::black_box(report);
            }
            _ => unreachable!(),
        }
    }
    println!(
        "{}",
        serde_json::json!({ "mode": mode, "queries": queries, "file_bytes": std::fs::metadata(path)?.len(),
        "iterations": iterations, "mean_ms": started.elapsed().as_secs_f64() * 1000.0 / iterations as f64 })
    );
    Ok(())
}
