//! Table and write relationships are selected from catalog identity and numeric detector facts.
use super::*;
use culprit_core::{FindingBloat, FindingLogWrite, RefinementDecision};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    DeadRows,
    ColdUpdates,
    LargeValues,
}
impl Policy {
    fn id(self) -> &'static str {
        match self {
            Self::DeadRows => "dead-rows-explain-bloat",
            Self::ColdUpdates => "cold-updates-explain-log",
            Self::LargeValues => "large-values-explain-log",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::DeadRows => "dead rows are the table's empty space",
            Self::ColdUpdates => "updates that aren't HOT explain their WAL",
            Self::LargeValues => "large values explain the log their writes make",
        }
    }
}
pub(super) struct Rule(pub Policy);
enum Evidence {
    Bloat(FindingBloat),
    Cold { query: String, rows: f64, log: FindingLogWrite, once: bool },
    Large,
}
struct Decision {
    from: usize,
    into: usize,
    evidence: Evidence,
}

/// A modifying plan or parsed SQL resolves to exactly one catalog table. An unqualified name shared by schemas is
/// insufficient evidence; a formatted query handle is never an identity.
fn written_table(s: &Snapshot, q: &QueryStat) -> Option<(&'static str, String)> {
    let (_, verb) = support::first_verb(support::main_statement(&q.text))?;
    if !matches!(verb, "insert" | "update" | "merge") {
        return None;
    }
    let targets = support::write_targets(q, s.server.engine);
    let tables: Vec<_> = s
        .tables
        .iter()
        .filter(|t| targets.iter().any(|target| support::names_table(target, &t.schema, &t.name)))
        .collect();
    match tables.as_slice() {
        [t] => Some((verb, t.qualified().to_ascii_lowercase())),
        _ => None,
    }
}
fn decide(policy: Policy, s: &Snapshot, findings: &[Finding]) -> Option<Decision> {
    for (from, f) in findings.iter().enumerate() {
        let table = finding_table(f);
        let (into, evidence) = match policy {
            Policy::DeadRows if f.check == "table-bloat" => {
                let Some(bloat) = &f.facts.bloat else { continue };
                if !s
                    .tables
                    .iter()
                    .find(|t| t.qualified().eq_ignore_ascii_case(&table))
                    .and_then(signals::dead_share)
                    .is_some_and(|dead| dead >= bloat.share * signals::DEAD_EXPLAINS_BLOAT)
                {
                    continue;
                }
                let Some(into) = position_on(findings, "dead-rows", &table) else { continue };
                (into, Evidence::Bloat(bloat.clone()))
            }
            Policy::ColdUpdates | Policy::LargeValues if f.check == "log-volume" => {
                let Some(queries) = merge::complete_queries(s, f) else { continue };
                let [q] = queries.as_slice() else { continue };
                let Some((verb, table)) = written_table(s, q) else { continue };
                let Some(log) = f.facts.log_write.clone() else { continue };
                if matches!(policy, Policy::ColdUpdates) {
                    if verb != "update" {
                        continue;
                    }
                    let Some(t) = s.tables.iter().find(|t| t.qualified().eq_ignore_ascii_case(&table)) else {
                        continue;
                    };
                    let Some(cold) = t
                        .updates
                        .zip(t.storage.as_ref().and_then(|storage| storage.hot_updates))
                        .map(|(updates, hot)| updates - hot)
                    else {
                        continue;
                    };
                    let Some(rows) = q.rows.filter(|rows| cold > 0.0 && *rows >= 0.5 * cold) else { continue };
                    let Some(into) = position_on(findings, "hot-updates", &table) else { continue };
                    (into, Evidence::Cold { query: q.id.clone(), rows, log, once: ran_once(q) })
                } else {
                    if !log.per_row.is_some_and(|bytes| bytes >= 8192.0) {
                        continue;
                    }
                    let Some(into) = position_on(findings, "large-rows", &table).filter(|&at| {
                        findings[at].facts.large_rows.is_some() && merge::complete_queries(s, &findings[at]).is_some()
                    }) else {
                        continue;
                    };
                    (into, Evidence::Large)
                }
            }
            _ => continue,
        };
        return Some(Decision { from, into, evidence });
    }
    None
}
impl Refine for Rule {
    fn name(&self) -> &str {
        self.0.label()
    }
    fn refine(&self, s: &Snapshot, findings: &mut Vec<Finding>) {
        let _ = self.refine_traced(s, findings);
    }
    fn refine_traced(&self, s: &Snapshot, findings: &mut Vec<Finding>) -> Vec<RefinementDecision> {
        let mut trace = Vec::new();
        let mut joined = BTreeSet::new();
        while let Some(d) = decide(self.0, s, findings) {
            trace.push(merge::trace(self.0.id(), findings, d.from, d.into));
            let a = merge::Absorption::decide(s, findings, d.into, d.from);
            a.apply_state(findings);
            if let Evidence::Cold { once: true, .. } = d.evidence {
                findings[d.into].severity = Severity::Low;
            }
            let target = &mut findings[d.into];
            let mut note = None;
            match d.evidence {
                Evidence::Bloat(bloat)=>target.caveats.push(format!(
                    "PostgreSQL's statistics also estimate about {} of {} ({}) as empty space: these dead rows. VACUUM makes it reusable; rewrite the table (pg_repack) only if the estimate stays high after a vacuum.",
                    format::bytes(bloat.bytes),leaf(&finding_table(&a.source)),format::percent(bloat.share))),
                Evidence::Cold {query,rows,log,once}=> {
                    let q=s.workload.as_ref().unwrap().query(&query).unwrap();
                    target.summary.push_str(&format!(
                        " {} changed {} of these rows and wrote {} of WAL for them{}: an update that isn't HOT logs the new row version and an entry in every index, and the first change to each page after a checkpoint logs the whole page.",
                        capitalize(&support::query_handle(q)),format::count(rows),format::bytes(log.bytes),log.per_row.map(|bytes|format!(", about {} a row",format::bytes(bytes))).unwrap_or_default()));
                    target.metrics.push(culprit_core::Metric::new("WAL its update wrote",log.bytes,culprit_core::Unit::Bytes));
                    if once {target.caveats.insert(0,format!("The update behind most of these ran once in {}: a backfill or a migration looks like this, and its share stays in the counts long after. It needs work only if updates like it keep coming.",s.workload.as_ref().unwrap().window_phrase()));}
                    note=Some(format!("Wrote {} of WAL.",format::bytes(log.bytes)));
                }
                Evidence::Large=>{joined.insert(target.id.clone());}
            }
            a.render(findings, note);
            a.finish(findings);
        }
        for f in findings.iter_mut().filter(|f| joined.contains(&f.id)) {
            let listed = merge::complete_queries(s, f).expect("complete merged query evidence");
            f.summary = signals::restate_large_rows(s, f, &listed).expect("retained table evidence");
            let table = finding_table(f);
            let writers: Vec<_> = listed
                .iter()
                .filter_map(|q| {
                    let (verb, written) = written_table(s, q)?;
                    let bytes = q.costs.as_ref()?.log_bytes.filter(|b| *b > 0.0)?;
                    (written == table).then_some((*q, verb, bytes))
                })
                .collect();
            if let Some(said) = log_of_writers(s, &writers) {
                f.summary.push_str(&said);
            }
            let total = writers.iter().map(|(_, _, bytes)| bytes).sum();
            match f.metrics.iter_mut().find(|m| m.label == "Log written by its writes") {
                Some(m) => m.value = total,
                None => f.metrics.push(culprit_core::Metric::new(
                    "Log written by its writes",
                    total,
                    culprit_core::Unit::Bytes,
                )),
            }
            for (q, _, bytes) in &writers {
                if let Some(r) = f.queries.iter_mut().find(|r| r.id == q.id && r.note.is_none()) {
                    r.note = Some(match per_row(q, *bytes) {
                        Some(b) => format!("Writes about {} of {} a row.", format::bytes(b), log_name(s)),
                        None => format!("Wrote {} of {}.", format::bytes(*bytes), log_name(s)),
                    });
                }
            }
        }
        trace
    }
}

/// What the engine calls its log, as a sentence names it.
fn log_name(snapshot: &Snapshot) -> &'static str {
    if snapshot.server.engine == Engine::Postgres { "WAL" } else { "transaction log" }
}

/// Log a statement wrote for each row it changed, when it counted them.
fn per_row(q: &QueryStat, bytes: f64) -> Option<f64> {
    q.rows.filter(|r| *r > 0.0).map(|rows| bytes / rows)
}

/// The log the writes of a large-rows table made, said once for all of them: one writer by its handle, several by what
/// they are ("77.2 MB by an insert (about 412 KB a row) and 15.3 MB by 2 updates (about 90.8 KB a row)").
fn log_of_writers(snapshot: &Snapshot, writers: &[(&QueryStat, &'static str, f64)]) -> Option<String> {
    const IN_FULL: &str = "the large values are logged in full, so batching the writes or dropping indexes won't shrink it; smaller or fewer values will.";
    let log = log_name(snapshot);
    let a_row = |bytes: f64, rows: Option<f64>| {
        rows.filter(|r| *r > 0.0).map(|r| format!("about {} a row", format::bytes(bytes / r)))
    };
    if let [(q, _, bytes)] = writers {
        return Some(format!(
            " {} wrote {} of {log} for them{}: {IN_FULL}",
            capitalize(&support::query_handle(q)),
            format::bytes(*bytes),
            a_row(*bytes, q.rows).map(|r| format!(", {r}")).unwrap_or_default()
        ));
    }
    // By kind of write, the kind that logged most first; a row count only where every one of them counted its rows.
    let mut kinds: Vec<(&'static str, usize, f64, Option<f64>)> = Vec::new();
    for (q, verb, bytes) in writers {
        match kinds.iter_mut().find(|(v, ..)| v == verb) {
            Some((_, n, b, rows)) => {
                *n += 1;
                *b += bytes;
                *rows = rows.zip(q.rows).map(|(a, r)| a + r);
            }
            None => kinds.push((verb, 1, *bytes, q.rows)),
        }
    }
    kinds.sort_by(|a, b| b.2.total_cmp(&a.2));
    let total: f64 = kinds.iter().map(|(_, _, b, _)| b).sum();
    let noun = |verb: &str, n: usize| match (verb, n) {
        ("insert", 1) => "an insert".to_string(),
        ("update", 1) => "an update".to_string(),
        (verb, 1) => format!("a {verb}"),
        (verb, n) => format!("{n} {verb}s"),
    };
    Some(match kinds.as_slice() {
        [] => return None,
        // Several writes of one kind (one alone is said by its handle above).
        [(verb, n, bytes, rows)] => format!(
            " The {n} {verb}s below logged {} of {log} for them{}: {IN_FULL}",
            format::bytes(*bytes),
            a_row(*bytes, *rows).map(|r| format!(", {r}")).unwrap_or_default()
        ),
        _ => {
            let parts: Vec<String> = kinds
                .iter()
                .map(|(verb, n, bytes, rows)| {
                    format!(
                        "{} by {}{}",
                        format::bytes(*bytes),
                        noun(verb, *n),
                        a_row(*bytes, *rows).map(|r| format!(" ({r})")).unwrap_or_default()
                    )
                })
                .collect();
            format!(
                " The {} statements below that write them logged {} of {log}: {}. {}",
                writers.len(),
                format::bytes(total),
                format::list(&parts),
                capitalize(IN_FULL)
            )
        }
    })
}
