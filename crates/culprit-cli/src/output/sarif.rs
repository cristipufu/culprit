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
                    "informationUri": "https://culpritsql.com",
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
    let (norm_sql, byte_lines) = normalize_sql_with_lines(sql);
    if let Some(after_tick) = f.summary.split('`').nth(1) {
        let full_needle = after_tick
            .trim()
            .trim_end_matches(';')
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        if !full_needle.is_empty()
            && let Some(pos) = norm_sql.find(&full_needle)
            && let Some(&line) = byte_lines.get(pos)
        {
            return Some(line);
        }
        let prefix = after_tick.split_whitespace().take(4).collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        if !prefix.is_empty()
            && let Some(pos) = norm_sql.find(&prefix)
            && let Some(&line) = byte_lines.get(pos)
        {
            return Some(line);
        }
    }
    let leaf = f.subject.name.rsplit('.').next().unwrap_or(&f.subject.name).to_ascii_lowercase();
    if !leaf.is_empty()
        && let Some(pos) = norm_sql.find(&leaf)
        && let Some(&line) = byte_lines.get(pos)
    {
        return Some(line);
    }
    None
}

fn parse_dollar_tag(sql: &str) -> Option<&str> {
    let rest = sql.strip_prefix('$')?;
    let end = rest.find('$')?;
    let tag = &rest[..end];
    if tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { Some(&sql[..end + 2]) } else { None }
}

fn normalize_sql_with_lines(sql: &str) -> (String, Vec<usize>) {
    let mut norm = String::with_capacity(sql.len());
    let mut byte_lines = Vec::with_capacity(sql.len());
    let mut line = 1usize;
    let mut chars = sql.char_indices().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut dollar_tag: Option<String> = None;
    let mut last_was_space = true;

    let push_lower = |ch: char, line_no: usize, norm: &mut String, byte_lines: &mut Vec<usize>| {
        for lc in ch.to_lowercase() {
            let len = lc.len_utf8();
            norm.push(lc);
            byte_lines.extend(std::iter::repeat_n(line_no, len));
        }
    };

    while let Some((idx, c)) = chars.next() {
        if c == '\n' {
            line += 1;
            if !last_was_space {
                norm.push(' ');
                byte_lines.push(line);
                last_was_space = true;
            }
            continue;
        }
        if let Some(ref tag) = dollar_tag {
            if c.is_whitespace() {
                if !last_was_space {
                    norm.push(' ');
                    byte_lines.push(line);
                    last_was_space = true;
                }
            } else {
                push_lower(c, line, &mut norm, &mut byte_lines);
                last_was_space = false;
            }
            if c == '$' && sql[idx..].starts_with(tag.as_str()) {
                for _ in 0..tag.chars().count().saturating_sub(1) {
                    if let Some((_, ch)) = chars.next() {
                        push_lower(ch, line, &mut norm, &mut byte_lines);
                    }
                }
                dollar_tag = None;
            }
            continue;
        }
        if (in_single || in_double) && c == '\\' {
            push_lower(c, line, &mut norm, &mut byte_lines);
            if let Some((_, next_c)) = chars.next() {
                if next_c == '\n' {
                    line += 1;
                }
                push_lower(next_c, line, &mut norm, &mut byte_lines);
            }
            last_was_space = false;
            continue;
        }
        if !in_single && !in_double {
            if c == '$'
                && let Some(tag) = parse_dollar_tag(&sql[idx..])
            {
                push_lower(c, line, &mut norm, &mut byte_lines);
                for _ in 0..tag.chars().count().saturating_sub(1) {
                    if let Some((_, ch)) = chars.next() {
                        push_lower(ch, line, &mut norm, &mut byte_lines);
                    }
                }
                last_was_space = false;
                dollar_tag = Some(tag.to_string());
                continue;
            }
            if c == '-' && chars.peek().map(|(_, ch)| *ch) == Some('-') {
                for (_, ch) in chars.by_ref() {
                    if ch == '\n' {
                        line += 1;
                        break;
                    }
                }
                if !last_was_space {
                    norm.push(' ');
                    byte_lines.push(line);
                    last_was_space = true;
                }
                continue;
            }
            if c == '/' && chars.peek().map(|(_, ch)| *ch) == Some('*') {
                chars.next();
                while let Some((_, ch)) = chars.next() {
                    if ch == '\n' {
                        line += 1;
                    } else if ch == '*' && chars.peek().map(|(_, next_ch)| *next_ch) == Some('/') {
                        chars.next();
                        break;
                    }
                }
                if !last_was_space {
                    norm.push(' ');
                    byte_lines.push(line);
                    last_was_space = true;
                }
                continue;
            }
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        }
        if c.is_whitespace() {
            if !last_was_space {
                norm.push(' ');
                byte_lines.push(line);
                last_was_space = true;
            }
        } else {
            push_lower(c, line, &mut norm, &mut byte_lines);
            last_was_space = false;
        }
    }
    (norm, byte_lines)
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
