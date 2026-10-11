//! Did the fix work? Compare the report from before a change with a fresh one, finding by finding: fixed, still there,
//! can't tell (the check couldn't run), and what's new. Pure, so the CLI, the app and agents read the same answer.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::finding::{Finding, Report};
use crate::format;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum VerifyStatus {
    /// In the earlier scan, gone from the new one, and the check ran.
    Fixed,
    /// Still reported by the new scan.
    StillPresent,
    /// The check couldn't run in the new scan, or the id is in neither scan.
    Unknown,
    /// Only the new scan reports it.
    New,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct VerifiedFinding {
    pub id: String,
    pub title: String,
    pub status: VerifyStatus,
    /// Impact in the earlier scan ("31% of DB time"), when it was there.
    pub before: Option<String>,
    /// Impact in the new scan, when it is still there.
    pub after: Option<String>,
    /// What to make of it: why a fixed query can still show up, what couldn't be checked.
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Verification {
    pub connection: String,
    pub before_taken_at: String,
    pub after_taken_at: String,
    /// One sentence: "2 of 3 fixed; 1 still present. 1 new finding."
    pub summary: String,
    pub findings: Vec<VerifiedFinding>,
    /// Findings only the new scan has (a new index that isn't used yet, a regression the change caused).
    pub new_findings: Vec<VerifiedFinding>,
}

/// Compare `after` (a fresh scan) with `before`. `ids` picks the findings to check; empty means every finding `before`
/// had.
pub fn compare_reports(before: &Report, after: &Report, ids: &[String]) -> Verification {
    let targets: Vec<String> =
        if ids.is_empty() { before.findings.iter().map(|f| f.id.clone()).collect() } else { ids.to_vec() };
    let find = |report: &'_ Report, id: &str| report.findings.iter().find(|f| f.id == id).cloned();
    let mut findings = Vec::new();
    for id in &targets {
        let (was, now) = (find(before, id), find(after, id));
        let item = match (was, now) {
            (Some(was), Some(now)) => VerifiedFinding {
                id: id.clone(),
                title: now.title.clone(),
                status: VerifyStatus::StillPresent,
                before: Some(was.impact.label.clone()),
                after: Some(now.impact.label.clone()),
                note: still_present_note(&was, &now, after),
            },
            (Some(was), None) => match locked(after, &was) {
                Some(missing) => VerifiedFinding {
                    id: id.clone(),
                    title: was.title.clone(),
                    status: VerifyStatus::Unknown,
                    before: Some(was.impact.label.clone()),
                    after: None,
                    note: Some(format!("The check couldn't run in the new scan ({missing}), so this can't be confirmed.")),
                },
                None => VerifiedFinding {
                    id: id.clone(),
                    title: was.title.clone(),
                    status: VerifyStatus::Fixed,
                    before: Some(was.impact.label.clone()),
                    after: None,
                    note: None,
                },
            },
            (None, Some(now)) => VerifiedFinding {
                id: id.clone(),
                title: now.title.clone(),
                status: VerifyStatus::StillPresent,
                before: None,
                after: Some(now.impact.label.clone()),
                note: Some("The earlier scan didn't report it; the new one does.".into()),
            },
            (None, None) => VerifiedFinding {
                id: id.clone(),
                title: id.clone(),
                status: VerifyStatus::Unknown,
                before: None,
                after: None,
                note: Some(
                    "Neither the latest scan nor the new one reports it: it may have been fixed before the latest scan. \
                     Current ids come from `culprit findings`."
                        .into(),
                ),
            },
        };
        findings.push(item);
    }
    let new_findings: Vec<VerifiedFinding> = after
        .findings
        .iter()
        .filter(|f| !before.findings.iter().any(|b| b.id == f.id) && !targets.contains(&f.id))
        .map(|f| VerifiedFinding {
            id: f.id.clone(),
            title: f.title.clone(),
            status: VerifyStatus::New,
            before: None,
            after: Some(f.impact.label.clone()),
            note: None,
        })
        .collect();
    Verification {
        connection: after.snapshot.connection.clone(),
        before_taken_at: before.snapshot.taken_at.clone(),
        after_taken_at: after.snapshot.taken_at.clone(),
        summary: summary(&findings, new_findings.len()),
        findings,
        new_findings,
    }
}

/// The checks that couldn't run for this finding in `report`, by capability: "Query Store".
fn locked(report: &Report, finding: &Finding) -> Option<String> {
    let lock = report.locked.iter().find(|l| l.check.id == finding.check)?;
    let names: Vec<String> = lock.missing.iter().map(|c| c.label().to_string()).collect();
    Some(if names.is_empty() { "a capability is missing".into() } else { format!("needs {}", format::list(&names)) })
}

fn still_present_note(was: &Finding, now: &Finding, after: &Report) -> Option<String> {
    if !now.queries.is_empty()
        && let Some(w) = after.snapshot.workload.as_ref()
    {
        let span = format::duration(w.window_hours * 3_600_000.0);
        return Some(if w.windowed {
            format!(
                "It is measured over the last {span} of workload, which still includes runs from before the change. \
                 Check again once that window has moved past it, or scan with a shorter window."
            )
        } else {
            format!(
                "{} counts since its statistics were reset ({span} ago), so runs from before the change still \
                 count. Reset the statement statistics after the change, or check again once newer runs dominate.",
                w.source
            )
        });
    }
    let (b, a) = (was.impact.score, now.impact.score);
    (b > 0.0 && a < b * 0.5).then(|| format!("Smaller than before: {} → {}.", was.impact.label, now.impact.label))
}

fn summary(findings: &[VerifiedFinding], new: usize) -> String {
    let n = |s: VerifyStatus| findings.iter().filter(|f| f.status == s).count();
    let (fixed, still, unknown) = (n(VerifyStatus::Fixed), n(VerifyStatus::StillPresent), n(VerifyStatus::Unknown));
    let mut parts = Vec::new();
    if findings.len() == 1 {
        parts.push(
            match findings[0].status {
                VerifyStatus::Fixed => "Fixed: the new scan no longer reports it",
                VerifyStatus::StillPresent => "Still present in the new scan",
                _ => "Can't tell from the new scan",
            }
            .to_string(),
        );
    } else {
        parts.push(format!("{fixed} of {} fixed", findings.len()));
        if still > 0 {
            parts.push(format!("{still} still present"));
        }
        if unknown > 0 {
            parts.push(format!("{unknown} can't be checked"));
        }
    }
    let mut out = parts.join("; ") + ".";
    if new > 0 {
        out.push_str(&format!(" {new} new finding{}.", if new == 1 { "" } else { "s" }));
    }
    out
}
