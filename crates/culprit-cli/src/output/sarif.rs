use std::collections::BTreeMap;

use culprit_core::{Drift, DriftKind, DriftPresence, Finding, Severity};
use serde_json::{Value, json};

pub(crate) fn findings_sarif(findings: &[&Finding], sql_text: Option<&str>, file_uri: Option<&str>) -> Value {
    let owned: Vec<Finding> = findings.iter().map(|f| (*f).clone()).collect();
    let batch = [(file_uri, sql_text, owned.as_slice())];
    findings_sarif_batches(&batch)
}

pub(crate) fn findings_sarif_batches(batches: &[(Option<&str>, Option<&str>, &[Finding])]) -> Value {
    let mut rules_by_id: BTreeMap<String, Value> = BTreeMap::new();
    let total_len: usize = batches.iter().map(|(_, _, fs)| fs.len()).sum();
    let mut results = Vec::with_capacity(total_len);

    for (file_uri, sql_text, findings) in batches {
        let uri = file_uri.unwrap_or(if sql_text.is_some() { "migration.sql" } else { "database" });
        for f in *findings {
            let rule_id = rule_id_for(f);
            rules_by_id.entry(rule_id.clone()).or_insert_with(|| {
                json!({
                    "id": rule_id,
                    "shortDescription": { "text": f.title },
                    "defaultConfiguration": { "level": sarif_level(f.severity) }
                })
            });

            let mut text = format!("{}\n\n{}", f.title, f.summary);
            if let Some(fix) = &f.fix {
                text.push_str(&format!("\n\nFix: {}", fix.summary));
                if let Some(sql) = &fix.sql {
                    text.push_str(&format!("\n```sql\n{sql}\n```"));
                }
            }

            let start_line = sql_text.and_then(|sql| locate_finding_line(sql, f)).unwrap_or(1);
            results.push(json!({
                "ruleId": rule_id,
                "level": sarif_level(f.severity),
                "fingerprints": { "culpritFindingId": f.id },
                "message": { "text": text },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": uri },
                        "region": { "startLine": start_line }
                    }
                }]
            }));
        }
    }

    sarif_envelope(rules_by_id.into_values().collect(), results)
}

pub(crate) fn drift_sarif(drift: &Drift) -> Value {
    let mut rules_by_id: BTreeMap<String, Value> = BTreeMap::new();
    let mut results = Vec::with_capacity(drift.items.len());
    let uri = format!("{}-vs-{}", drift.a.connection, drift.b.connection);

    for item in &drift.items {
        let kind_slug = match item.kind {
            DriftKind::Index => "index",
            DriftKind::Table => "table",
            DriftKind::Column => "column",
            DriftKind::Setting => "setting",
            DriftKind::Version => "version",
        };
        let rule_id = format!("drift:{kind_slug}");
        let level = match item.presence {
            DriftPresence::Renamed => "note",
            DriftPresence::Different | DriftPresence::OnlyA | DriftPresence::OnlyB => "warning",
        };
        rules_by_id.entry(rule_id.clone()).or_insert_with(|| {
            json!({
                "id": rule_id,
                "shortDescription": { "text": format!("Schema or setting drift ({kind_slug})") },
                "defaultConfiguration": { "level": level }
            })
        });

        let mut text = item.title.clone();
        if let (Some(sql), Some(target)) = (&item.sql, &item.sql_for) {
            text.push_str(&format!("\n\nTo align on {target}:\n```sql\n{sql}\n```"));
        }
        results.push(json!({
            "ruleId": rule_id,
            "level": level,
            "fingerprints": { "culpritDriftSubject": format!("{kind_slug}:{}", item.subject) },
            "message": { "text": text },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": uri },
                    "region": { "startLine": 1 }
                }
            }]
        }));
    }

    sarif_envelope(rules_by_id.into_values().collect(), results)
}

fn sarif_envelope(rules: Vec<Value>, results: Vec<Value>) -> Value {
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "Culprit",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://culprit.dev",
                    "rules": rules
                }
            },
            "results": results
        }]
    })
}

fn rule_id_for(f: &Finding) -> String {
    if let Some(rest) = f.id.strip_prefix("sql-lint:")
        && let Some((kind, _)) = rest.split_once(':')
    {
        return format!("sql-lint:{kind}");
    }
    f.check.clone()
}

fn sarif_level(severity: Severity) -> &'static str {
    match severity {
        Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// Find the 1-based line number in `sql` where the statement referenced by `f` starts.
fn locate_finding_line(sql: &str, f: &Finding) -> Option<usize> {
    if let Some(after_tick) = f.summary.split('`').nth(1) {
        let prefix = after_tick.split_whitespace().take(4).collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        if !prefix.is_empty() {
            for (idx, line) in sql.lines().enumerate() {
                let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
                if normalized.contains(&prefix) {
                    return Some(idx + 1);
                }
            }
        }
    }
    let leaf = f.subject.name.rsplit('.').next().unwrap_or(&f.subject.name).to_ascii_lowercase();
    if !leaf.is_empty() {
        for (idx, line) in sql.lines().enumerate() {
            if line.to_ascii_lowercase().contains(&leaf) {
                return Some(idx + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::Report;

    #[test]
    fn sarif_output_maps_lint_sql_findings_to_migration_file_and_line() {
        let report: Report = serde_json::from_str(include_str!("../../../../fixtures/pg-demo.report.json")).unwrap();
        let mut s = report.snapshot.clone();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let sql = format!("-- migration header\nSELECT 1;\nCREATE INDEX idx_blocking ON {table} (created_at);\n");
        let findings = culprit_engine::lint_sql(&s, &sql);
        let refs: Vec<&Finding> = findings.iter().collect();
        let sarif = findings_sarif(&refs, Some(&sql), Some("migrations/0042_add_idx.sql"));
        assert_eq!(sarif["version"], "2.1.0");
        let result = &sarif["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "sql-lint:blocking-index");
        assert_eq!(result["level"], "warning");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "migrations/0042_add_idx.sql"
        );
        assert_eq!(result["locations"][0]["physicalLocation"]["region"]["startLine"], 3);

        // Multi-file batches preserve each file's URI and line number independently.
        let sql2 = format!("-- second file\nCREATE INDEX idx_blocking_two ON {table} (created_at);\n");
        let findings2 = culprit_engine::lint_sql(&s, &sql2);
        let batches = [
            (Some("migrations/0001_first.sql"), Some(sql.as_str()), findings.as_slice()),
            (Some("migrations/0002_second.sql"), Some(sql2.as_str()), findings2.as_slice()),
        ];
        let multi_sarif = findings_sarif_batches(&batches);
        let results = multi_sarif["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "migrations/0001_first.sql"
        );
        assert_eq!(results[0]["locations"][0]["physicalLocation"]["region"]["startLine"], 3);
        assert_eq!(
            results[1]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "migrations/0002_second.sql"
        );
        assert_eq!(results[1]["locations"][0]["physicalLocation"]["region"]["startLine"], 2);
    }
}
