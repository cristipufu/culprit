//! Redact literal values before a plan, a comparison or a finding leaves the machine (a PR, a ticket, a chat). Strings
//! and numbers in SQL text and predicates become `'?'` and `?`; names (tables, columns, indexes) and parameter markers
//! (`@P1`, `$1`, `?`, `:name`) stay, so the text still explains the query. Each engine's quoting is followed: SQL
//! Server's `[names]` and `N'…'`, Postgres's `"names"`, `E'…'` and `$$…$$`, MySQL's `` `names` ``, `"…"` strings and
//! backslash escapes.

use crate::finding::Finding;
use crate::model::{Engine, Snapshot};

mod plan;
mod sql;
#[cfg(test)]
mod tests;

pub use plan::{redact_native, redact_plan};
pub use sql::{redact_prose, redact_sql, redact_strings, strip_database};

fn opaque(value: &str) -> String {
    use sha2::{Digest, Sha256};
    // Preserve our own tokens when a shared artifact is read again.
    if value.strip_prefix("redacted-").is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())) {
        value.into()
    } else {
        let digest: String = Sha256::digest(value.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
        format!("redacted-{digest}")
    }
}

/// Physical paths are not public finding identifiers. Logical catalog names remain recognizable.
pub fn shareable_name(value: &str) -> String {
    if value.contains(['/', '\\']) { opaque(value) } else { value.into() }
}

/// Keep database labels and equality relationships, without retaining connection endpoints or file paths.
pub fn redact_identity(identity: &mut crate::DatabaseIdentity) {
    use crate::DatabaseLocation;
    match &mut identity.location {
        DatabaseLocation::Server { host, port, instance } => {
            if !host.trim().is_empty() {
                let canonical = host.trim().trim_end_matches('.').to_ascii_lowercase();
                *host = if canonical.starts_with("redacted-") && *port == 1 {
                    opaque(&canonical)
                } else {
                    opaque(&format!("{canonical}:{port}"))
                };
            }
            if *port != 0 {
                *port = 1;
            }
            *instance = instance.as_deref().map(opaque);
        }
        DatabaseLocation::Socket { directory, port } => {
            if !directory.is_empty() {
                *directory = if directory.starts_with("redacted-") && *port == 1 {
                    opaque(directory)
                } else {
                    opaque(&format!("{directory}:{port}"))
                };
            }
            if *port != 0 {
                *port = 1;
            }
        }
        DatabaseLocation::File { path } if !path.is_empty() => *path = opaque(path),
        _ => {}
    }
    identity.incarnation = identity.incarnation.as_deref().map(opaque);
}

pub fn redact_measurement(measurement: &mut crate::MeasurementContext) {
    redact_identity(&mut measurement.database);
}

fn redact_reference(reference: &mut crate::ScanReference) {
    if let Some(database) = &mut reference.database {
        redact_identity(database);
    }
}

pub fn redact_baseline(reference: &mut crate::BaselineReference) {
    redact_reference(&mut reference.scan);
    if let Some(measurement) = &mut reference.measurement {
        redact_measurement(measurement);
    }
}

/// A snapshot to build text that leaves the machine from (an export, a digest posted to chat): statements, plans,
/// query samples and deadlock statements go through the SQL lexer, so their literals become placeholders, and the
/// parameter values plans were compiled for are dropped. Findings checked from it can only quote what is left, so
/// nothing has to be picked back out of their prose: re-check this, don't redact the findings of the original.
pub fn redact_snapshot(snapshot: &Snapshot) -> Snapshot {
    let engine = snapshot.server.engine;
    let mut s = snapshot.clone();
    if let Some(identity) = &mut s.identity {
        redact_identity(identity);
    }
    for note in &mut s.notes {
        note.message = "Collection details omitted from shared data.".into();
    }
    for source in &mut s.sources {
        if source.reason.is_some() {
            source.reason = Some("Collection details omitted from shared data.".into());
        }
    }
    for missing in &mut s.server.missing {
        missing.reason = "This capability was not collected; diagnostic details omitted from shared data.".into();
        missing.fix = None;
    }
    for failure in &mut s.operations.log_failures {
        failure.statement = redact_sql(&failure.statement, engine);
    }
    for table in &mut s.tables {
        table.definition = table.definition.as_deref().map(|sql| redact_sql(sql, engine));
        if let Some(storage) = &mut table.storage {
            for option in &mut storage.options {
                if let Some(bound) = option.strip_prefix("partition_bound=") {
                    *option = format!("partition_bound={}", redact_partition_bound(bound, engine));
                }
            }
        }
    }
    for index in &mut s.indexes {
        if index.filter_facts.is_none()
            && let Some(filter) = &index.filter
        {
            index.filter_facts = Some(crate::IndexFilterFacts {
                fingerprint: opaque(filter),
                matching_queries: snapshot
                    .queries()
                    .iter()
                    .filter(|q| crate::index_filter::matches(&index.table, filter, &q.text))
                    .map(|q| q.id.clone())
                    .collect(),
                equalities: crate::index_filter::filter_equalities(filter).map(|pairs| {
                    pairs.into_iter().map(|(column, value)| (column, redact_sql(&value, engine))).collect()
                }),
            });
        }
        if let Some(facts) = &mut index.filter_facts {
            facts.fingerprint = opaque(&facts.fingerprint);
            if let Some(equalities) = &mut facts.equalities {
                for (_, value) in equalities {
                    *value = redact_sql(value, engine);
                }
            }
        }
        index.filter = index.filter.as_deref().map(|sql| redact_sql(sql, engine));
        index.definition = index.definition.as_deref().map(|sql| redact_sql(sql, engine));
    }
    for column in &mut s.columns {
        column.default = column.default.as_deref().map(|sql| redact_sql(sql, engine));
    }
    for reference in s
        .escalations_before
        .iter_mut()
        .filter_map(|b| b.reference.as_mut())
        .chain(s.contention_before.iter_mut().filter_map(|b| b.reference.as_mut()))
        .chain(s.sizes_before.iter_mut().filter_map(|b| b.reference.as_mut()))
        .chain(s.evictions_before.iter_mut().filter_map(|b| b.reference.as_mut()))
        .chain(s.operations.counters.iter_mut().filter_map(|c| c.before.as_mut()).filter_map(|b| b.reference.as_mut()))
        .chain(s.operations.sqlite.iter_mut().filter_map(|c| c.before.as_mut()).filter_map(|b| b.reference.as_mut()))
    {
        redact_reference(reference);
    }
    if let Some(measurement) = s.evictions_before.as_mut().and_then(|b| b.measurement.as_mut()) {
        redact_measurement(measurement);
    }
    if let Some(w) = &mut s.workload {
        if let Some(measurement) = &mut w.measurement {
            redact_measurement(measurement);
        }
        for reference in &mut w.history_inputs {
            redact_baseline(reference);
        }
        for q in &mut w.queries {
            q.text = redact_sql(&q.text, engine);
            q.plan = q.plan.as_ref().map(redact_plan);
            for p in &mut q.plans {
                p.compiled = None;
            }
        }
    }
    if let Some(tuning) = &mut s.tuning {
        for recommendation in &mut tuning.recommendations {
            recommendation.script = recommendation.script.as_deref().map(|sql| redact_sql(sql, engine));
            recommendation.reason = recommendation.reason.as_deref().map(|t| redact_prose(t, engine));
        }
        for forced in &mut tuning.forced_plans {
            forced.last_failure_reason =
                forced.last_failure_reason.as_deref().map(|_| "Failure details omitted from shared data.".into());
        }
        for hint in &mut tuning.hints {
            hint.hint = redact_sql(&hint.hint, engine);
            hint.last_failure_reason =
                hint.last_failure_reason.as_deref().map(|_| "Failure details omitted from shared data.".into());
        }
    }
    if let Some(p) = &mut s.pressure {
        if let Some(transaction) = &mut p.oldest_transaction {
            transaction.program = None;
        }
        for file in &mut p.files {
            file.name = shareable_name(&file.name);
        }
        for replica in &mut p.replicas {
            replica.error = replica.error.as_deref().map(|e| {
                if e.is_empty() { "".into() } else { "Replication error details omitted from shared data.".into() }
            });
        }
    }
    for space in &mut s.operations.spaces {
        space.name = shareable_name(&space.name);
        for part in &mut space.parts {
            part.name = shareable_name(&part.name);
        }
    }
    for f in &mut s.query_families {
        f.sample = redact_sql(&f.sample, engine);
    }
    if let Some(report) = &mut s.deadlocks {
        for d in &mut report.deadlocks {
            for p in &mut d.processes {
                p.statement = p.statement.as_deref().map(|t| redact_sql(t, engine));
                // Who ran it: an Entra login is a person's email address.
                p.login = None;
                p.host = None;
                p.application = None;
            }
            // The headline, explanation and fixes quote the processes: written again from what is left.
            crate::deadlock::classify(engine, d);
        }
        if report.note.is_some() {
            report.note = Some(
                engine
                    .spec()
                    .deadlock_limitation()
                    .unwrap_or("Deadlock collection was limited; details omitted from shared data.")
                    .into(),
            );
        }
    }
    s
}

/// Redact literal values in a partition bound expression (`FOR VALUES IN ('tenant_a', 'tenant_b')`) while keeping
/// `DEFAULT`, `MAXVALUE`, and ISO date/timestamp range bounds (`'2026-10-01'`) so partition expiry checks still work
/// on shared snapshots.
fn redact_partition_bound(bound: &str, engine: Engine) -> String {
    if bound.eq_ignore_ascii_case("default") || bound.eq_ignore_ascii_case("maxvalue") {
        return bound.to_string();
    }
    let mut dates: Vec<String> = Vec::new();
    let mut masked = String::with_capacity(bound.len());
    let mut rest = bound;
    while let Some(open) = rest.find('\'') {
        masked.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('\'') {
            let candidate = &after_open[..close];
            if is_iso_date_or_timestamp(candidate) {
                let idx = dates.len();
                dates.push(format!("'{candidate}'"));
                masked.push_str(&format!("__culprit_date_{idx}__"));
                rest = &after_open[close + 1..];
                continue;
            }
        }
        masked.push('\'');
        rest = after_open;
    }
    masked.push_str(rest);
    let mut redacted = redact_sql(&masked, engine);
    for (idx, date_lit) in dates.iter().enumerate() {
        redacted = redacted.replace(&format!("__culprit_date_{idx}__"), date_lit);
    }
    redacted
}

fn is_iso_date_or_timestamp(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.len() < 10 || trimmed.len() > 35 {
        return false;
    }
    let b = trimmed.as_bytes();
    let is_ymd = b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit();
    if !is_ymd {
        return false;
    }
    if trimmed.len() == 10 {
        crate::format::epoch_seconds(&format!("{trimmed}T00:00:00Z")).is_some()
    } else {
        crate::format::epoch_seconds(trimmed).is_some()
    }
}

/// A second layer over [`redact_snapshot`], for text from a finding that wasn't checked from a redacted snapshot (a
/// plug-in check an export can't re-run): its prose redacted with [`redact_prose`], which reads SQL out of prose by
/// heuristics and so can't be the only layer. Fix SQL can also quote catalog predicates or plugin-supplied values.
pub fn redact_finding(finding: &Finding, engine: Engine) -> Finding {
    let prose = |t: &str| redact_prose(t, engine);
    let mut f = finding.clone();
    f.title = prose(&f.title);
    f.summary = prose(&f.summary);
    f.caveats = f.caveats.iter().map(|c| prose(c)).collect();
    f.verify = f.verify.iter().map(|v| prose(v)).collect();
    for m in &mut f.metrics {
        m.text = m.text.as_deref().map(prose);
    }
    for q in &mut f.queries {
        q.note = q.note.as_deref().map(prose);
    }
    if let Some(fix) = &mut f.fix {
        fix.summary = prose(&fix.summary);
        fix.cost = fix.cost.as_deref().map(prose);
        fix.sql = fix.sql.as_deref().map(|sql| redact_sql(sql, engine));
    }
    f
}
