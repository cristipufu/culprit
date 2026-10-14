//! How a snapshot becomes a report. Four stages, in this order, each a list you can read top to bottom and extend:
//!
//! 1. **Prepare**: make the snapshot say what the database says now, before anything reads it. Query Store keeps a
//!    plan's first XML, so a plan captured before a column changed type still shows the old conversion; marking it
//!    here keeps every check and every plan insight from reporting a mismatch the table no longer has.
//! 2. **Detect**: every check that applies to the engine and can run with what the login sees looks at the snapshot
//!    for one kind of problem ([`crate::all`]). A check that needs a capability the scan lacks is listed as locked,
//!    with the reason, instead of running on missing data.
//! 3. **Refine**: rules over every check's findings ([`crate::refine`]). Two findings about one cause become one, and a
//!    finding that rests on statements whose cost is behind them (they stopped running, or ran once) says so and ranks
//!    low. A rule here applies to every check on every engine: a false flag several checks could raise is fixed once.
//! 4. **Rank and summarize**: active findings first, by impact, then the headline.
//!
//! Before all of this, a provider's sources (Query Store, pg_stat_statements, the index usage views…) fill the snapshot,
//! and nothing after it touches a connection. An embedding application adds its own checks, preparation steps or
//! rules with [`Pipeline::with_check`], [`Pipeline::with_prepare`] and [`Pipeline::with_refine`]; they run, rank and
//! summarize exactly like the built-in ones.

use std::sync::Arc;

use culprit_core::{Check, Finding, LockedCheck, Report, Snapshot};

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
}

/// A built-in step or rule: a name and a function.
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

impl Refine for Step<fn(&Snapshot, &mut Vec<Finding>)> {
    fn name(&self) -> &str {
        self.name
    }

    fn refine(&self, snapshot: &Snapshot, findings: &mut Vec<Finding>) {
        (self.run)(snapshot, findings)
    }
}

/// The built-in preparation steps, in the order they run.
fn preparation() -> Vec<Arc<dyn Prepare>> {
    let step = |name: &'static str, run: fn(&mut Snapshot)| Arc::new(Step { name, run }) as Arc<dyn Prepare>;
    vec![
        step("plans captured before a column changed type", culprit_core::insights::mark_snapshot),
        step("MySQL's possible keys named with their columns", culprit_core::insights::mark_snapshot_keys),
    ]
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
        for check in &self.checks {
            let info = check.info();
            if !info.engines.contains(&snapshot.server.engine) {
                continue;
            }
            let missing = crate::missing_for(&info, &snapshot.server);
            if !missing.is_empty() {
                locked.push(LockedCheck { check: info, missing });
                continue;
            }
            findings.extend(check.run(&snapshot));
        }
        for rule in &self.refine {
            rule.refine(&snapshot, &mut findings);
        }
        rank(&mut findings);
        let summary = crate::summarize(&snapshot, &findings);
        Report { snapshot, findings, locked, summary }
    }
}

/// Active findings first, then by impact, then by severity.
fn rank(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.inactive.cmp(&b.inactive).then(b.impact.score.total_cmp(&a.impact.score)).then(b.severity.cmp(&a.severity))
    });
}
