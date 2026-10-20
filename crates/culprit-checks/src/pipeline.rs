//! How a snapshot becomes a report. Four stages, in this order, each a list you can read top to bottom and extend:
//!
//! 1. **Prepare**: make the snapshot say what the database says now, before anything reads it. Query Store keeps a
//!    plan's first XML, so a plan captured before a column changed type still shows the old conversion; marking it
//!    here keeps every check and every plan insight from reporting a mismatch the table no longer has.
//! 2. **Detect**: every check that applies to the engine and can run with what the login sees looks at the snapshot
//!    for one kind of problem ([`crate::all`]). A check that needs a capability the scan lacks is listed as locked,
//!    with the reason, instead of running on missing data.
//! 3. **Refine**: rules over every check's findings (`refine.rs`). Two findings about one cause become one, and a
//!    finding that rests on statements whose cost is behind them (they stopped running, or ran once) says so and ranks
//!    low. A rule here applies to every check on every engine: a false flag several checks could raise is fixed once.
//! 4. **Rank and summarize**: active findings first, by impact, then the headline.
//!
//! Before all of this, a provider's sources (Query Store, pg_stat_statements, the index usage views…) fill the snapshot,
//! and nothing after it touches a connection. An embedding application adds its own checks, preparation steps or
//! rules with [`Pipeline::with_check`], [`Pipeline::with_prepare`] and [`Pipeline::with_refine`]; they run, rank and
//! summarize exactly like the built-in ones.

use std::sync::Arc;

use culprit_core::{Check, CollectorNote, Engine, Finding, LockedCheck, Report, Snapshot, WaitStat};

/// A step that corrects the snapshot before any check reads it. It changes what the data says only where the data
/// contradicts itself or the database (a plan older than the table it describes), never to hide a problem.
pub trait Prepare: Send + Sync {
    /// What it corrects, in a few words.
    fn name(&self) -> &str;
    fn prepare(&self, snapshot: &mut Snapshot);
}

/// A rule over all the findings, after every check ran: merge two findings about one cause, or say that a finding
/// rests on statements that are history.
pub trait Refine: Send + Sync {
    /// What it fixes, in a few words.
    fn name(&self) -> &str;
    fn refine(&self, snapshot: &Snapshot, findings: &mut Vec<Finding>);

    /// Apply the rule and expose its decisions. Existing extensions keep their original implementation; the default
    /// records invocation without pretending to know their policy. A structured rule decides before rendering prose.
    fn refine_traced(&self, snapshot: &Snapshot, findings: &mut Vec<Finding>) -> Vec<culprit_core::RefinementDecision> {
        self.refine(snapshot, findings);
        vec![culprit_core::RefinementDecision {
            rule: self.name().into(),
            finding: None,
            outcome: culprit_core::RefinementOutcome::RuleApplied,
            queries: Vec::new(),
            tables: Vec::new(),
        }]
    }
}

/// A built-in preparation step: a name and a function.
pub(crate) struct Step<F> {
    pub name: &'static str,
    pub run: F,
}

impl Prepare for Step<fn(&mut Snapshot)> {
    fn name(&self) -> &str {
        self.name
    }

    fn prepare(&self, snapshot: &mut Snapshot) {
        (self.run)(snapshot)
    }
}

/// The built-in preparation steps, in the order they run.
fn preparation() -> Vec<Arc<dyn Prepare>> {
    let step = |name: &'static str, run: fn(&mut Snapshot)| Arc::new(Step { name, run }) as Arc<dyn Prepare>;
    vec![
        step("measurement context identifies this database and its clock", measurement_context),
        step("index availability and maintenance follow the engine", |s| {
            for i in &mut s.indexes {
                i.state = Some(if i.is_disabled {
                    s.server.engine.spec().inactive_index_state(&s.server)
                } else {
                    culprit_core::IndexState::Active
                });
                if i.maintained.is_none() {
                    i.maintained = match i.state {
                        Some(culprit_core::IndexState::Disabled) => Some(false),
                        Some(culprit_core::IndexState::Invalid) => None,
                        _ => Some(true),
                    };
                }
            }
        }),
        step("CTE writes named for their base table", culprit_core::sql::resolve_write_targets),
        step("plans captured before a column changed type", culprit_core::insights::mark_snapshot),
        step("MySQL's possible keys named with their columns", culprit_core::insights::mark_snapshot_keys),
        step("plan operators marked with their index's kind", culprit_core::insights::mark_snapshot_index_kinds),
        step("saved storage counters retain their source scope", storage_scope),
        step("health counters graded only on the evidence", crate::pressure::grade_on_evidence),
        step("PostgreSQL's WAL time kept out of statement time", statement_time_without_wal),
        step("MariaDB's lock time unknown where it counts the whole run", lock_time_that_counts_the_run),
        step("a scan's filter judged by what its runs measured", crate::indexes::mark_measured_kept),
    ]
}

fn measurement_context(snapshot: &mut Snapshot) {
    use culprit_core::{MeasurementScope, MeasurementSource};
    let Some(context) = snapshot.workload.as_ref().and_then(|w| w.measurement.clone()) else { return };
    let reason = if context.scope != MeasurementScope::Database {
        Some(
            "The workload source does not isolate this database; its measurements cannot assess this database."
                .to_string(),
        )
    } else if !snapshot.measurement_consistency().allows_comparison() {
        Some("The workload's recorded database identity does not match this snapshot; its measurements were left unassessed.".to_string())
    } else {
        None
    };
    if let Some(message) = reason {
        snapshot.workload = None;
        snapshot.notes.push(CollectorNote { collector: "workload".into(), message });
        return;
    }
    if let Some(workload) = &mut snapshot.workload {
        workload.windowed = context.period.is_windowed();
        workload.window_hours = context.period.hours().unwrap_or(0.0);
        if let culprit_core::MeasurementPeriod::Window { span: Some(span), .. } = &context.period {
            workload.window_start = Some(span.start.clone());
            workload.window_end = Some(span.end.clone());
        }
        if let MeasurementSource::Workload(kind) = context.source {
            workload.kind = Some(kind);
        }
    }
}

/// Older snapshots did not save these facts. Reconstruct only scope and operation kind; an unknown source reset
/// stays unknown instead of borrowing the database, bgwriter or server-start clock.
fn storage_scope(s: &mut Snapshot) {
    let engine = s.server.engine;
    let major = s.server.version.split('.').next().and_then(|n| n.parse::<u32>().ok());
    for f in s.pressure.iter_mut().flat_map(|p| p.files.iter_mut()) {
        let scope = match engine {
            Engine::Postgres if major.is_some_and(|v| v < 16) && f.kind == culprit_core::FileKind::Data => {
                Some(culprit_core::FileScope::Database)
            }
            Engine::Postgres if f.kind == culprit_core::FileKind::Data && major.is_none() => None,
            Engine::Postgres | Engine::Mysql => Some(culprit_core::FileScope::Server),
            Engine::Mssql if f.database.is_some() => Some(culprit_core::FileScope::Database),
            _ => None,
        };
        if f.scope.is_none() {
            f.scope = scope;
        }
        if engine == Engine::Postgres && f.kind == culprit_core::FileKind::Log {
            f.write_operation.get_or_insert(culprit_core::FileWriteOperation::Flush);
        }
    }
}

/// With the slow query log writing to a table, MariaDB reports a logged statement's whole run as its lock time
/// ([`crate::signals::lock_time_counts_the_run`]): 49,963.3 ms of lock in a run of 49,963.9 ms. The real lock time isn't
/// kept, so it is unknown (`None`, and no lock wait), never read as waiting by any check (what a heavy query waited
/// on, how long reads waited on writes). The slow log's own total of lock time is made of such runs too. A note says so.
fn lock_time_that_counts_the_run(snapshot: &mut Snapshot) {
    let Some(workload) = snapshot.workload.as_ref() else { return };
    let inflated: Vec<String> = workload
        .queries
        .iter()
        .filter(|q| crate::signals::lock_time_counts_the_run(snapshot, q))
        .filter(|q| q.spread.as_ref().is_some_and(|s| s.lock_ms.is_some()) || q.waits.iter().any(is_lock_time))
        .map(|q| q.id.clone())
        .collect();
    if inflated.is_empty() {
        return;
    }
    if let Some(workload) = snapshot.workload.as_mut() {
        for q in workload.queries.iter_mut().filter(|q| inflated.contains(&q.id)) {
            if let Some(spread) = q.spread.as_mut() {
                spread.lock_ms = None;
            }
            q.waits.retain(|w| !is_lock_time(w));
        }
    }
    snapshot.waits.retain(|w| w.wait_type.as_deref() != Some("lock time (slow query log)"));
    let statements =
        if inflated.len() == 1 { "1 statement".to_string() } else { format!("{} statements", inflated.len()) };
    snapshot.notes.push(CollectorNote {
        collector: "workload".into(),
        message: format!(
            "MariaDB counts the whole run of a statement the slow query log writes to its table as lock time, so the \
             lock time of {statements} is left out as unknown rather than read as waiting."
        ),
    });
}

/// The lock time MySQL and MariaDB report for a statement (its digest's, or the slow log's), kept as a Lock wait.
fn is_lock_time(w: &WaitStat) -> bool {
    w.category == "Lock" && w.wait_type.as_deref().is_some_and(|t| t.starts_with("lock time"))
}

/// Scans by older versions split PostgreSQL's statement time into disk I/O, WAL write and sync, and the rest. WAL time is
/// the whole server's, spent mostly by the WAL writer and by commits after a statement's timing ends: no share of
/// statement time. It goes back to the rest, as a scan now reads it (`culprit_postgres` `history_waits`), and without
/// disk I/O to set apart the rest is all there is, which says nothing.
fn statement_time_without_wal(snapshot: &mut Snapshot) {
    let wal = |w: &WaitStat| w.category == "Log" && w.wait_type.as_deref().is_some_and(|t| t.starts_with("WAL write"));
    if snapshot.server.engine != Engine::Postgres {
        return;
    }
    // Saved scans need the same protection as freshly collected ones. Parallel I/O timings or counters with
    // different coverage cannot be partitioned into exclusive shares of statement wall time.
    let io: f64 = snapshot.waits.iter().filter(|w| w.category == "IO").map(|w| w.wait_ms).sum();
    if snapshot.workload.as_ref().is_some_and(|w| io > w.total_time_ms) {
        snapshot.waits.clear();
        snapshot.server.capabilities.retain(|c| *c != culprit_core::Capability::WaitStats);
        snapshot.notes.push(CollectorNote {
            collector: "waits".into(),
            message: "Recorded I/O time exceeds statement execution time; parallel or differently covered timing cannot be shown as exclusive shares of execution time. Per-query I/O totals remain available.".into(),
        });
        return;
    }
    if !snapshot.waits.iter().any(wal) {
        return;
    }
    let ms: f64 = snapshot.waits.iter().filter(|w| wal(w)).map(|w| w.wait_ms).sum();
    snapshot.waits.retain(|w| !wal(w));
    if !snapshot.waits.iter().any(|w| w.category == "IO" && w.wait_ms > 0.0) {
        snapshot.waits.clear();
        return;
    }
    match snapshot.waits.iter_mut().find(|w| w.category == "Other") {
        Some(other) => other.wait_ms += ms,
        None => snapshot.waits.push(WaitStat {
            category: "Other".into(),
            wait_type: Some("CPU, locks and untimed waits".into()),
            wait_ms: ms,
            tasks: None,
        }),
    }
    snapshot.waits.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
}

/// The stages and what runs in each.
#[derive(Clone)]
pub struct Pipeline {
    prepare: Vec<Arc<dyn Prepare>>,
    checks: Vec<Arc<dyn Check>>,
    refine: Vec<Arc<dyn Refine>>,
}

impl Pipeline {
    /// Culprit's own steps, checks and rules.
    pub fn builtin() -> Self {
        Pipeline {
            prepare: preparation(),
            checks: crate::all().into_iter().map(Arc::from).collect(),
            refine: crate::refine::rules(),
        }
    }

    /// Adds a check after the built-in ones. A check whose id is already taken is ignored, so a plug-in can't replace
    /// a built-in check's findings.
    pub fn with_check(mut self, check: Arc<dyn Check>) -> Self {
        let id = check.info().id;
        if !self.checks.iter().any(|c| c.info().id == id) {
            self.checks.push(check);
        }
        self
    }

    /// Adds a preparation step after the built-in ones.
    pub fn with_prepare(mut self, step: Arc<dyn Prepare>) -> Self {
        self.prepare.push(step);
        self
    }

    /// Adds a rule after the built-in ones, before ranking.
    pub fn with_refine(mut self, rule: Arc<dyn Refine>) -> Self {
        self.refine.push(rule);
        self
    }

    /// Every check, in run order.
    pub fn checks(&self) -> &[Arc<dyn Check>] {
        &self.checks
    }

    /// Prepare, detect, refine, rank and summarize.
    pub fn run(&self, mut snapshot: Snapshot) -> Report {
        for step in &self.prepare {
            step.prepare(&mut snapshot);
        }
        let mut findings = Vec::new();
        let mut locked = Vec::new();
        let mut assessments = Vec::new();
        for check in &self.checks {
            let info = check.info();
            if !info.engines.contains(&snapshot.server.engine) {
                continue;
            }
            let missing = crate::missing_for(&info, &snapshot.server);
            let mut assessment = culprit_core::CheckAssessment {
                check: info.id.clone(),
                title: info.title.clone(),
                status: culprit_core::CheckStatus::Assessed,
                reason: None,
            };
            if !missing.is_empty() {
                assessment.status = culprit_core::CheckStatus::Unavailable;
                assessment.reason = Some(
                    missing
                        .iter()
                        .map(|c| {
                            snapshot
                                .server
                                .missing
                                .iter()
                                .find(|m| m.capability == *c)
                                .map(|m| m.reason.clone())
                                .unwrap_or_else(|| format!("{c:?} was not collected"))
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                assessments.push(assessment);
                locked.push(LockedCheck { check: info, missing });
                continue;
            }
            use culprit_core::{CheckEvidence, CheckStatus};
            match check.evidence(&snapshot) {
                CheckEvidence::Available => {}
                CheckEvidence::Unavailable(reason) => {
                    assessment.status = CheckStatus::Unavailable;
                    assessment.reason = Some(reason);
                }
                CheckEvidence::Insufficient(reason) => {
                    assessment.status = CheckStatus::InsufficientEvidence;
                    assessment.reason = Some(reason);
                }
                CheckEvidence::NotApplicable(reason) => {
                    assessment.status = CheckStatus::NotApplicable;
                    assessment.reason = Some(reason);
                }
            }
            let run = !matches!(assessment.status, CheckStatus::Unavailable | CheckStatus::NotApplicable);
            assessments.push(assessment);
            if !run {
                continue;
            }
            findings.extend(check.run(&snapshot));
        }
        let mut refinement = Vec::new();
        for rule in &self.refine {
            refinement.extend(rule.refine_traced(&snapshot, &mut findings));
        }
        for f in &mut findings {
            f.actionability = if f.inactive {
                culprit_core::Actionability::History
            } else if f.actionability == culprit_core::Actionability::OptionalImprovement {
                if f.severity == culprit_core::Severity::Low {
                    culprit_core::Actionability::OptionalImprovement
                } else {
                    culprit_core::Actionability::NeedsAttention
                }
            } else {
                f.actionability
            };
        }
        rank(&mut findings);
        crate::pressure::grade_findings(&mut snapshot, &findings);
        let summary = crate::summarize(&snapshot, &findings);
        crate::annotate_plans(&mut snapshot);
        for assessment in &mut assessments {
            if findings.iter().any(|f| f.check == assessment.check) {
                assessment.status = culprit_core::CheckStatus::Findings;
            }
        }
        Report { snapshot, findings, locked, summary, assessments, refinement }
    }
}

/// Active findings first, then by severity, then by impact. Severity already weighs a finding's share of database time
/// with the time behind it: a Low finding with a large share of a quiet database's few milliseconds (or a warning the
/// plan says is harmless) never tops a Medium one.
fn rank(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.actionability
            .cmp(&b.actionability)
            .then(a.inactive.cmp(&b.inactive))
            .then(b.severity.cmp(&a.severity))
            .then(b.impact.score.total_cmp(&a.impact.score))
    });
}
