use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Capability, Engine, Snapshot};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Category {
    Indexes,
    Queries,
    Maintenance,
    Configuration,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Indexes => "Indexes",
            Category::Queries => "Queries",
            Category::Maintenance => "Maintenance",
            Category::Configuration => "Configuration",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// What a person can do with a finding, independently of its severity. Historical and informational evidence stays
/// available, but contributes neither an opportunity nor an alert. Older reports default to optional advice until
/// the current checks read their evidence again.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum Actionability {
    NeedsAttention,
    #[default]
    OptionalImprovement,
    Information,
    History,
}

impl Actionability {
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::NeedsAttention | Self::OptionalImprovement)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsAttention => "Needs attention",
            Self::OptionalImprovement => "Optional improvement",
            Self::Information => "Information",
            Self::History => "History",
        }
    }
}

/// What a later scan can establish. A cumulative observation cannot disappear as proof of a fix: resetting or
/// losing its counters can remove it too. Embedding checks can opt into the same verification contract.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum VerificationBasis {
    #[default]
    Rescan,
    CounterChange,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SubjectKind {
    Query,
    Table,
    Index,
    Database,
    Setting,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Subject {
    pub kind: SubjectKind,
    pub name: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Unit {
    Ms,
    Count,
    Bytes,
    Ratio,
    Rows,
    Pages,
    Days,
    Text,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Metric {
    pub label: String,
    pub value: f64,
    pub unit: Unit,
    pub text: Option<String>,
}

impl Metric {
    pub fn new(label: impl Into<String>, value: f64, unit: Unit) -> Self {
        Metric { label: label.into(), value, unit, text: None }
    }

    pub fn text(label: impl Into<String>, text: impl Into<String>) -> Self {
        Metric { label: label.into(), value: 0.0, unit: Unit::Text, text: Some(text.into()) }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct Impact {
    /// 0–100, the ranking key across all findings.
    pub score: f64,
    /// Share of total database time spent in the affected queries, 0–1.
    pub db_time_share: Option<f64>,
    /// Database time per day spent in the affected queries.
    pub time_ms_per_day: Option<f64>,
    /// Short human label for the impact column, e.g. "31% of DB time" or "6.1 GB unused".
    pub label: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct QueryRef {
    pub id: String,
    pub plan_node: Option<u32>,
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Fix {
    pub summary: String,
    pub sql: Option<String>,
    pub cost: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Finding {
    pub id: String,
    pub check: String,
    pub category: Category,
    pub severity: Severity,
    #[serde(default)]
    pub actionability: Actionability,
    pub confidence: Confidence,
    pub title: String,
    pub summary: String,
    pub subject: Subject,
    pub impact: Impact,
    pub metrics: Vec<Metric>,
    pub queries: Vec<QueryRef>,
    pub fix: Option<Fix>,
    pub verify: Vec<String>,
    #[serde(default)]
    pub verification_basis: VerificationBasis,
    pub caveats: Vec<String>,
    /// When the affected queries last ran, for query findings.
    #[serde(default)]
    pub last_seen: Option<String>,
    /// The affected queries stopped running well before the end of the window: a deploy may have fixed or replaced them.
    #[serde(default)]
    pub inactive: bool,
    #[serde(default, skip_serializing_if = "crate::FindingFacts::is_empty")]
    pub facts: crate::FindingFacts,
}

impl Finding {
    pub fn is_actionable(&self) -> bool {
        !self.inactive && self.actionability.is_actionable()
    }

    /// A check's explicit decision that its evidence supplies context and no justified change.
    pub fn informational(&mut self) {
        self.actionability = Actionability::Information;
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CheckInfo {
    pub id: String,
    pub title: String,
    pub category: Category,
    pub engines: Vec<Engine>,
    pub requires: Vec<Capability>,
    pub description: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct LockedCheck {
    pub check: CheckInfo,
    pub missing: Vec<Capability>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Summary {
    pub opportunities: u32,
    pub by_category: Vec<CategoryCount>,
    pub by_severity: Vec<SeverityCount>,
    /// Share of database time touched by the three highest-ranked query findings.
    pub top_share: Option<f64>,
    pub headline: String,
    /// Why the scan has no statement statistics ("Query Store is OFF on this database."), `None` when it has them: one
    /// sentence every surface shows instead of deciding for itself which capability explains it.
    #[serde(default)]
    pub no_workload: Option<String>,
    /// How the runs of the statements a rate a day would misstate are said ([`crate::Workload::cadence`]): those that
    /// came in bursts, and in a window of a day or more those that ran fewer times than it has days. Every surface
    /// that lists statements says these instead of a rate a day ("750 runs on Oct 11, 15:51–15:55 UTC"); the others'
    /// runs came a day at a time, or are counted in a window under a day.
    #[serde(default)]
    pub runs_said: Vec<RunsSaid>,
    /// What Culprit's own scans measured of the statements they did, on a source of running totals with a day or more
    /// of them ([`crate::Workload::scans_of`]): their share, runs a day and average as every surface states them
    /// ([`crate::Workload::cost_of`]). A surface that lists statements (the app) says these; the others' figures are
    /// their totals over the window.
    #[serde(default)]
    pub over_scans: Vec<crate::model::StatementCost>,
}

/// How one statement's runs are said where a rate a day would misstate them ([`Summary::runs_said`]).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct RunsSaid {
    /// The statement's id in the workload.
    pub query: String,
    /// "750 runs on Oct 11, 15:51–15:55 UTC", "120K runs on Oct 11 and Oct 17", "3 runs in the last 6 days".
    pub runs: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CategoryCount {
    pub category: Category,
    pub count: u32,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SeverityCount {
    pub severity: Severity,
    pub count: u32,
}

/// A snapshot together with everything derived from it. This is what a scan produces and what gets stored.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Report {
    pub snapshot: Snapshot,
    pub findings: Vec<Finding>,
    pub locked: Vec<LockedCheck>,
    pub summary: Summary,
    #[serde(default)]
    pub assessments: Vec<crate::check::CheckAssessment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refinement: Vec<crate::RefinementDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub provenance: Option<crate::AnalysisProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub evidence: Option<crate::ReportEvidence>,
}

impl Report {
    /// Older reports retain their prepared snapshot as the only available input, without claiming it is original.
    pub fn analysis_input(&self) -> &Snapshot {
        self.evidence.as_ref().map_or(&self.snapshot, |e| e.input.as_ref())
    }

    /// Why a missing finding cannot establish a clean result for this check. A partial source can still yield
    /// positive findings, so a `Findings` assessment with a reason retains that incomplete-evidence account.
    /// Reports saved before assessments existed retain their capability-lock interpretation.
    pub fn check_absence_problem(&self, check: &str) -> Option<String> {
        if let Some(lock) = self.locked.iter().find(|l| l.check.id == check) {
            let names: Vec<_> = lock.missing.iter().map(|c| c.label().to_string()).collect();
            return Some(if names.is_empty() {
                "A capability is missing.".into()
            } else {
                format!("Needs {}.", crate::format::list(&names))
            });
        }
        let Some(assessment) = self.assessments.iter().find(|a| a.check == check) else {
            return (!self.assessments.is_empty()).then(|| "This check was not run in the new scan.".into());
        };
        use crate::CheckStatus;
        match assessment.status {
            CheckStatus::Unavailable | CheckStatus::InsufficientEvidence => {
                Some(assessment.reason.clone().unwrap_or_else(|| "The required evidence is incomplete.".into()))
            }
            CheckStatus::Findings => assessment.reason.clone(),
            CheckStatus::Assessed | CheckStatus::NotApplicable => None,
        }
    }

    /// A finding reclassified under another cause did not disappear because a fix was verified.
    pub fn finding_absence_problem(&self, finding: &str, check: &str, queries: &[&str]) -> Option<String> {
        self.finding_absence_problem_with_baseline(finding, check, queries, None)
    }

    /// A retained baseline can establish recovery even after the earlier plan leaves the current window.
    pub fn finding_absence_problem_with_baseline(
        &self,
        finding: &str,
        check: &str,
        queries: &[&str],
        baseline: Option<&crate::PlanBaseline>,
    ) -> Option<String> {
        if let Some(baseline) = baseline {
            // This bypasses only the missing historical comparison, never a locked, unrun or failed check.
            if queries != [baseline.query.as_str()]
                || self.locked.iter().any(|l| l.check.id == check)
                || !self.assessments.iter().any(|a| a.check == check && a.status != crate::CheckStatus::Unavailable)
            {
                return Some("The plan-regression check could not assess the affected statement.".into());
            }
            return baseline.recovery_problem(&self.snapshot).or_else(|| self.refinement_absence_problem(finding));
        }
        if self.assessments.iter().any(|a| {
            a.check == check
                && !a.assessed_queries.is_empty()
                && queries.iter().any(|q| !a.assessed_queries.iter().any(|id| id == q))
        }) {
            return Some("The affected statement was not measured sufficiently in the new scan.".into());
        }
        let measured = !queries.is_empty()
            && !self.locked.iter().any(|l| l.check.id == check)
            && self.assessments.iter().any(|a| {
                a.check == check
                    && matches!(a.status, crate::CheckStatus::InsufficientEvidence | crate::CheckStatus::Findings)
                    && queries.iter().all(|q| a.assessed_queries.iter().any(|id| id == q))
            });
        self.check_absence_problem(check).filter(|_| !measured).or_else(|| self.refinement_absence_problem(finding))
    }

    fn refinement_absence_problem(&self, finding: &str) -> Option<String> {
        self.refinement
                .iter()
                .any(|decision| {
                    decision.finding.as_deref() == Some(finding)
                        && matches!(
                            decision.outcome,
                            crate::RefinementOutcome::Absorbed { .. } | crate::RefinementOutcome::Covered { .. }
                        )
                })
                .then(|| "This finding was combined with another observation during analysis; its disappearance does not verify a fix.".into())
    }
}
