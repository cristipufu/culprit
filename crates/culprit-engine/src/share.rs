//! Sharing and the verify loop on top of the workspace: compare plans with what the latest scan knows, export findings,
//! reports, plans and comparisons, and check whether fixes worked.

use culprit_core::{
    CompareContext, Error, Plan, PlanDiff, Result, SnapshotOptions, Verification, compare_reports, plan_diff_in,
};

use crate::Workspace;
use crate::export::{self, ExportFile, ExportFormat, ExportOptions};

impl Workspace {
    /// What the latest scan of `name` knows that explains plan differences (optimizer statistics). Empty when there's
    /// no scan.
    pub fn compare_context(&self, name: &str) -> CompareContext {
        match self.latest_snapshot(name) {
            Ok(Some(snapshot)) => CompareContext { statistics: snapshot.statistics, taken_at: Some(snapshot.taken_at) },
            _ => CompareContext::default(),
        }
    }

    /// Compare two plans; with `connection`, scenarios can name stale statistics from its latest scan.
    pub fn diff_plans(&self, a: &Plan, name_a: &str, b: &Plan, name_b: &str, connection: Option<&str>) -> PlanDiff {
        let context = connection.map(|c| self.compare_context(c)).unwrap_or_default();
        plan_diff_in(a, name_a, b, name_b, &context)
    }

    /// A finding from the latest scan as Markdown (for a PR) or a self-contained HTML page.
    pub fn export_finding(
        &self,
        name: &str,
        id: &str,
        format: ExportFormat,
        options: &ExportOptions,
    ) -> Result<ExportFile> {
        let report = self.latest_report(name, None)?.ok_or_else(|| {
            Error::not_found(format!("'{name}' hasn't been scanned yet"))
                .with_hint(format!("Run `culprit scan {name}` first"))
        })?;
        let finding = report.findings.iter().find(|f| f.id == id).ok_or_else(|| {
            Error::not_found(format!("No finding '{id}' in the latest report for {name}"))
                .with_hint(format!("List the current ids with `culprit findings {name}`"))
        })?;
        let stem = export::file_stem(&finding.title);
        Ok(match format {
            ExportFormat::Html => ExportFile {
                name: format!("{stem}.html"),
                contents: export::finding_html(&report, finding, options),
                format,
            },
            _ => ExportFile {
                name: format!("{stem}.md"),
                contents: export::finding_markdown(&report, finding, options),
                format: ExportFormat::Markdown,
            },
        })
    }

    /// Every finding of the latest scan.
    pub fn export_report(&self, name: &str, format: ExportFormat, options: &ExportOptions) -> Result<ExportFile> {
        let report = self.latest_report(name, None)?.ok_or_else(|| {
            Error::not_found(format!("'{name}' hasn't been scanned yet"))
                .with_hint(format!("Run `culprit scan {name}` first"))
        })?;
        Ok(match format {
            ExportFormat::Html => ExportFile {
                name: "culprit-report.html".into(),
                contents: export::report_html(&report, options),
                format,
            },
            _ => ExportFile {
                name: "culprit-report.md".into(),
                contents: export::report_markdown(&report, options),
                format: ExportFormat::Markdown,
            },
        })
    }

    /// A plan of a query from the latest scan, with the original plan text when the engine keeps it (Query Store).
    pub async fn export_query_plan(
        &self,
        name: &str,
        query_id: &str,
        plan_id: Option<&str>,
        format: ExportFormat,
        options: &ExportOptions,
    ) -> Result<ExportFile> {
        let plan = self.query_plan(name, query_id, plan_id).await?;
        let title = match plan_id {
            Some(id) => format!("Plan {id} of query {query_id}"),
            None => format!("Plan of query {query_id}"),
        };
        export_plan(&plan, &title, format, options)
    }

    /// Re-scan `name` with the same window as its latest scan and compare, finding by finding. `ids` empty checks every
    /// finding the earlier scan had.
    pub async fn verify(&self, name: &str, ids: &[String]) -> Result<Verification> {
        let before = self.latest_report(name, None)?.ok_or_else(|| {
            Error::not_found(format!("'{name}' hasn't been scanned yet, so there's nothing to compare with"))
                .with_hint(format!("Scan before the change (`culprit scan {name}`), then verify after it ships"))
        })?;
        let options = before.snapshot.options.clone().unwrap_or_else(SnapshotOptions::default);
        let after = self.scan(name, &options).await?;
        Ok(compare_reports(&before, &after, ids))
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn comparisons_read_prepared_statistics_without_replaying_retained_evidence() {
        let dir = std::env::temp_dir().join(format!("culprit-compare-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let workspace = Workspace::open_in(&dir);
        let report: culprit_core::Report =
            serde_json::from_str(include_str!("../../../fixtures/mssql-demo.report.json")).unwrap();
        let saved = workspace.paths().reports_dir(&report.snapshot.connection);
        crate::reports::save(&saved, &report).unwrap();
        let path = saved.join(format!("{}.json", report.snapshot.id));
        let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["_culprit_store"] =
            serde_json::json!({"version": 2, "input": {"changes": []}, "collected": null, "recorded_changes": []});
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(workspace.latest_report(&report.snapshot.connection, None).is_err());
        let context = workspace.compare_context(&report.snapshot.connection);
        assert_eq!(context.taken_at.as_deref(), Some(report.snapshot.taken_at.as_str()));
        assert_eq!(
            serde_json::to_value(context.statistics).unwrap(),
            serde_json::to_value(&report.snapshot.statistics).unwrap()
        );
        assert!(workspace.compare_context("missing").taken_at.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A plan as Markdown, HTML, or its own text (.sqlplan, EXPLAIN JSON or text) when it kept it.
pub fn export_plan(plan: &Plan, title: &str, format: ExportFormat, options: &ExportOptions) -> Result<ExportFile> {
    let stem = export::file_stem(title);
    Ok(match format {
        ExportFormat::Html => {
            ExportFile { name: format!("{stem}.html"), contents: export::plan_html(plan, title, options), format }
        }
        ExportFormat::Markdown => {
            ExportFile { name: format!("{stem}.md"), contents: export::plan_markdown(plan, title, options), format }
        }
        ExportFormat::Native => {
            let contents = export::plan_native(plan, options).ok_or_else(|| {
                Error::invalid("This plan has no original text to save")
                    .with_hint("Plans read from a scan keep only Culprit's model; open the query's plan or export HTML or Markdown")
            })?;
            ExportFile { name: format!("{stem}.{}", export::native_extension(plan)), contents, format }
        }
    })
}

/// Two plans compared, as Markdown or HTML.
pub fn export_diff(
    a: &Plan,
    b: &Plan,
    diff: &PlanDiff,
    name_a: &str,
    name_b: &str,
    format: ExportFormat,
    options: &ExportOptions,
) -> Result<ExportFile> {
    let stem = export::file_stem(&format!("compare {name_a} {name_b}"));
    Ok(match format {
        ExportFormat::Html => ExportFile {
            name: format!("{stem}.html"),
            contents: export::diff_html(a, b, diff, name_a, name_b, options),
            format,
        },
        _ => ExportFile {
            name: format!("{stem}.md"),
            contents: export::diff_markdown(a, b, diff, name_a, name_b, options),
            format: ExportFormat::Markdown,
        },
    })
}
