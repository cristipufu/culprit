//! Rules that hold for every finding of every demo fixture, read the way the app opens it: not one reported example,
//! the rule itself (a finding on statements that stopped gives no SQL to create anything, a verify names a number a
//! later scan can show, …). A new rule adds a test here that iterates over [`demo_reports`].

use culprit_core::Report;

/// Every demo fixture's report, by name, as `culprit_engine::reading` opens it (the same files as the golden test).
pub(crate) fn demo_reports() -> Vec<(String, Report)> {
    crate::golden::demo_files()
        .into_iter()
        .map(|(name, path)| {
            let report = culprit_engine::reading::open(&path).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            (name, report)
        })
        .collect()
}

#[test]
fn every_demo_fixture_opens() {
    let reports = demo_reports();
    assert!(reports.iter().all(|(_, r)| !r.findings.is_empty()), "every demo has findings");
}

#[test]
fn every_demo_finding_explains_how_to_verify_its_advice() {
    for (name, report) in demo_reports() {
        for finding in &report.findings {
            assert!(
                !finding.verify.is_empty(),
                "{name} {} leaves the app and agent without a verification step",
                finding.id
            );
        }
    }
}

/// The browser loads these files directly; golden readings re-analyze them, which alone cannot catch stale UI data.
#[test]
fn browser_demo_outputs_match_current_analysis() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let sample =
        |name: &str| -> serde_json::Value { serde_json::from_slice(&std::fs::read(root.join(name)).unwrap()).unwrap() };
    let dashboards = sample("dashboard.sample.json");
    let advice = sample("index-advice.sample.json");
    let insights = sample("insights.sample.json");
    let refresh = "refresh the browser demo with `python3 dev/refresh-demo.py`";
    assert_browser_json(&sample("checks.sample.json"), &serde_json::json!(culprit_checks::infos()), refresh);
    for (name, path) in crate::golden::demo_files().into_iter().filter(|(name, _)| name.ends_with(".report")) {
        let saved = culprit_engine::reports::load(&path).unwrap();
        let current = culprit_engine::reading::open(&path).unwrap();
        let connection = &current.snapshot.connection;
        let context = format!("{name}: {refresh}");
        assert_browser_json(
            &serde_json::json!([saved.findings, saved.summary, saved.locked]),
            &serde_json::json!([current.findings, current.summary, current.locked]),
            &context,
        );
        // CLI navigation commands are added by the CLI, while the desktop uses tile keys.
        let mut dashboard = dashboards[connection].clone();
        for tile in dashboard["tiles"].as_array_mut().unwrap() {
            tile["next"] = serde_json::Value::Null;
        }
        assert_browser_json(&dashboard, &serde_json::json!(culprit_engine::dashboard_report(&current)), &context);
        assert_browser_json(&advice[connection], &serde_json::json!(culprit_checks::index_advice(&current)), &context);
        let by_query: serde_json::Map<String, serde_json::Value> = current
            .snapshot
            .queries()
            .iter()
            .filter_map(|q| Some((q.id.clone(), serde_json::json!(q.plan.as_ref()?.insights))))
            .collect();
        assert_browser_json(&insights[connection], &serde_json::Value::Object(by_query), &context);
    }
}

/// Ignore only floating-point round-trip noise; report the first different field instead of pages of JSON.
fn assert_browser_json(saved: &serde_json::Value, current: &serde_json::Value, path: &str) {
    use serde_json::Value;
    match (saved, current) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>(), "{path}");
            for (key, value) in a {
                assert_browser_json(value, &b[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: array length");
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                assert_browser_json(a, b, &format!("{path}/{index}"));
            }
        }
        (Value::Number(a), Value::Number(b)) if a.is_f64() || b.is_f64() => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0), "{path}: {a} != {b}");
        }
        _ => assert_eq!(saved, current, "{path}"),
    }
}

#[test]
fn no_stopped_finding_promises_a_fix_when_only_its_table_grows() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        // The saved demos mostly keep running. Exercise the rule on each fixture after its statements have been
        // silent for two days, with no trend buckets that would contradict that history.
        let mut snapshot = report.snapshot;
        let Some(w) = snapshot.workload.as_mut() else { continue };
        let end = chrono::DateTime::parse_from_rfc3339(w.window_end.as_deref().unwrap_or(&snapshot.taken_at)).unwrap();
        w.window_hours = 96.0;
        for q in &mut w.queries {
            q.first_seen = Some((end - chrono::Duration::hours(96)).to_rfc3339());
            q.last_seen = Some((end - chrono::Duration::hours(48)).to_rfc3339());
            q.trend = None;
        }
        let report = culprit_checks::analyze(snapshot);
        for f in report.findings.iter().filter(|f| f.inactive) {
            seen += 1;
            assert!(
                f.verify.iter().all(|v| !v.contains("once ") || v.contains("runs again") || v.contains("is gone")),
                "{name} {}: {:?}",
                f.id,
                f.verify
            );
            let code = culprit_checks::agent_brief(&report, f);
            assert!(
                !brief_section(&code, "What to do in the codebase").contains("After any change is live"),
                "{name} {}: {code}",
                f.id
            );
        }
    }
    assert!(seen > 0);
}

#[test]
fn a_plan_note_never_uses_pages_alone_as_proof_of_a_row_misestimate() {
    for (name, report) in demo_reports() {
        for q in report.snapshot.workload.iter().flat_map(|w| &w.queries) {
            let Some(plan) = &q.plan else { continue };
            if matches!(
                culprit_core::insights::cheap_plan_runs(plan, Some(q)),
                culprit_core::insights::CheapPlanRuns::MoreReads { .. }
            ) {
                let note = culprit_checks::plan_shares_note(plan, Some(q)).unwrap();
                assert!(!note.contains("costs rest on an estimate of"), "{name} {}: {note}", q.id);
            }
        }
    }
}

#[test]
fn briefs_keep_lock_holders_as_the_fix_when_their_sql_only_logs_waits() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        for f in report.findings.iter().filter(|f| !f.inactive && f.check == "lock-time") {
            seen += 1;
            let mut diagnostic = f.clone();
            diagnostic.fix.as_mut().unwrap().sql =
                Some("ALTER SYSTEM SET log_lock_waits = 'on';\nSELECT pg_reload_conf();".into());
            let brief = culprit_checks::agent_brief(&report, &diagnostic);
            let code = brief_section(&brief, "What to do in the codebase");
            assert!(code.contains("keep those transactions short"), "{name} {}: {code}", f.id);
            assert!(!code.contains("an ORM change or a query rewrite"), "{name} {}: {code}", f.id);
        }
    }
    assert!(seen > 0);
}

// ── tables: what index and table findings, their fixes and their briefs say ──

/// A text's sentences as the refine stage compares them (lower case, spaces collapsed, no final stop): split after a
/// full stop, a question or an exclamation mark that a space and a capital, a digit or a quote follow.
fn sentence_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut start = 0;
    let mut push = |s: &str| {
        let key = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let key = key.trim_end_matches(['.', '!', '?', ':', ';']).to_lowercase();
        if !key.is_empty() {
            out.push(key);
        }
    };
    for w in chars.windows(3) {
        let [(at, end), (_, space), (_, next)] = [w[0], w[1], w[2]];
        if matches!(end, '.' | '?' | '!')
            && space == ' '
            && (next.is_uppercase() || next.is_ascii_digit() || matches!(next, '`' | '"' | '“'))
        {
            push(&text[start..at + 1]);
            start = at + 1;
        }
    }
    push(&text[start..]);
    out
}

/// The paragraphs under a brief's `## <heading>`, up to the next heading.
fn brief_section<'a>(brief: &'a str, heading: &str) -> &'a str {
    let Some(at) = brief.find(&format!("## {heading}\n")) else { return "" };
    let rest = &brief[at + heading.len() + 4..];
    &rest[..rest.find("\n## ").unwrap_or(rest.len())]
}

#[test]
fn no_caveat_repeats_the_summary_or_the_fix() {
    for (name, report) in demo_reports() {
        for f in &report.findings {
            let fix = f.fix.as_ref().map(|x| x.summary.as_str()).unwrap_or_default();
            let said: std::collections::HashSet<String> =
                sentence_keys(&f.summary).into_iter().chain(sentence_keys(fix)).collect();
            // A caveat that quotes a statement (a line break) is left as it is.
            for c in f.caveats.iter().filter(|c| !c.contains('\n')) {
                for s in sentence_keys(c) {
                    assert!(!said.contains(&s), "{name} {}: the caveat repeats the summary or the fix: {c}", f.id);
                }
            }
            // The brief says the same: its caveats repeat nothing its other sections said.
            let brief = culprit_checks::agent_brief(&report, f);
            let said: std::collections::HashSet<String> = ["What's happening", "Suggested fix"]
                .iter()
                .flat_map(|h| brief_section(&brief, h).lines().filter(|l| !l.starts_with("```")))
                .flat_map(sentence_keys)
                .collect();
            for line in brief_section(&brief, "Caveats").lines().filter_map(|l| l.strip_prefix("- ")) {
                for s in sentence_keys(line) {
                    assert!(!said.contains(&s), "{name} {}: the brief's caveat repeats it: {line}", f.id);
                }
            }
        }
    }
}

#[test]
fn an_event_session_is_one_session_created_once_and_stopped() {
    // Every finding that asks for the same session asks for the same one: created only where it isn't defined, started
    // only where it isn't running (a second fix's SQL runs as well as the first), and the SQL says how to drop it.
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let mut defined: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
        for f in &report.findings {
            let Some(sql) = f.fix.as_ref().and_then(|x| x.sql.as_deref()) else { continue };
            for (at, _) in sql.match_indices("CREATE EVENT SESSION ") {
                seen += 1;
                let session = sql[at + "CREATE EVENT SESSION ".len()..].split_whitespace().next().unwrap_or_default();
                assert!(
                    sql[..at].trim_end().ends_with(&format!("WHERE name = N'{session}')")),
                    "{name} {}: {session} is created only where it isn't defined yet:\n{sql}",
                    f.id
                );
                assert!(
                    sql.contains("STATE = START") && sql.contains(&format!("DROP EVENT SESSION {session}")),
                    "{name} {}: the SQL says how to stop {session}:\n{sql}",
                    f.id
                );
                let definition = sql[at..].split(';').next().unwrap_or_default().to_string();
                if let Some((other, first)) = defined.insert(session.to_string(), (definition.clone(), f.id.clone())) {
                    assert_eq!(other, definition, "{name}: {first} and {} define {session} two ways", f.id);
                }
            }
            let brief = culprit_checks::agent_brief(&report, f);
            if sql.contains("CREATE EVENT SESSION") {
                assert!(brief.contains("remove it with the DROP"), "{name} {}: the brief says how to stop it", f.id);
            }
        }
    }
    assert!(seen > 0, "a demo fixture starts an event session");
}

#[test]
fn this_index_is_said_only_beside_sql_that_creates_it() {
    for (name, report) in demo_reports() {
        for f in &report.findings {
            let Some(fix) = &f.fix else { continue };
            if fix.cost.as_deref().is_some_and(|c| c.contains("this index")) {
                let sql = fix.sql.as_deref().unwrap_or_default().to_ascii_uppercase();
                assert!(
                    sql.contains("INDEX") && (sql.contains("CREATE") || sql.contains("ADD")),
                    "{name} {}: \"this index\" with no SQL that creates one: {:?}",
                    f.id,
                    fix.cost
                );
            }
        }
        // The index advisor says it the same way.
        for c in culprit_checks::index_advice(&report).candidates {
            if c.write_cost.as_deref().is_some_and(|w| w.contains("this index")) {
                assert!(c.sql.is_some(), "{name} {}.{}({:?}): {:?}", c.schema, c.table, c.keys, c.write_cost);
            }
        }
    }
}

#[test]
fn a_findings_share_counts_every_statement_it_lists() {
    for (name, report) in demo_reports() {
        let Some(workload) = report.snapshot.workload.as_ref() else { continue };
        for f in &report.findings {
            let Some(share) = f.impact.db_time_share else { continue };
            let listed: f64 = f
                .queries
                .iter()
                .filter_map(|r| workload.query(&r.id))
                .map(|q| culprit_checks::query_share(workload, f, q))
                .sum();
            assert!(
                listed <= share + 0.002,
                "{name} {}: its share ({share:.4}) leaves out statements it lists ({listed:.4})",
                f.id
            );
        }
    }
}

#[test]
fn briefs_point_at_tables_not_indexes_and_at_every_table_of_the_finding() {
    for (name, report) in demo_reports() {
        for f in &report.findings {
            let brief = culprit_checks::agent_brief(&report, f);
            let code = brief_section(&brief, "What to do in the codebase");
            // The application reads and writes tables; an index is the database's.
            assert!(!code.contains("writes the index"), "{name} {}: {code}", f.id);
            // A deadlock is about the order of statements on every table it names.
            if let Some((_, tables)) = f.id.strip_prefix("deadlocks:").and_then(|r| r.split_once(':')) {
                for table in tables.split('+').filter(|t| *t != "unknown") {
                    assert!(code.contains(table), "{name} {}: the brief leaves out {table}: {code}", f.id);
                }
            }
            // A setting where nothing manages the server's configuration is still made somewhere.
            if code.contains("manages the server's configuration") {
                assert!(code.contains("Where nothing manages it"), "{name} {}: {code}", f.id);
            }
            // Whose transaction was rolled back is a session, never a bare login.
            for m in f.metrics.iter().filter(|m| m.label == "Rolled back") {
                let who = m.text.as_deref().unwrap_or_default();
                assert!(who.starts_with("session ") || who.starts_with("a session of "), "{name} {}: {who}", f.id);
            }
        }
    }
}

#[test]
fn index_findings_list_and_verify_what_the_index_serves() {
    for (name, report) in demo_reports() {
        let Some(workload) = report.snapshot.workload.as_ref() else { continue };
        for f in &report.findings {
            // A table that's gone, or statements that stopped, have no current plan.
            if f.inactive {
                assert!(!f.metrics.iter().any(|m| m.label == "Current plan"), "{name} {}", f.id);
            }
            // scan-heavy on MySQL: a statement that examines a sliver of the table a run reaches it through an index.
            if f.check == "scan-heavy" && report.snapshot.server.engine == culprit_core::Engine::Mysql {
                let rows = report.snapshot.tables.iter().find(|t| t.qualified() == f.subject.name).and_then(|t| t.rows);
                for q in f.queries.iter().filter_map(|r| workload.query(&r.id)) {
                    if let (Some(examined), Some(rows)) = (q.reads_per_execution(), rows) {
                        assert!(examined >= rows * 0.1, "{name} {}: {} examines {examined} rows a run", f.id, q.id);
                    }
                }
            }
            // An unindexed foreign key whose share comes from statements says how to see the index serve them.
            if f.check == "unindexed-fk" && f.impact.db_time_share.is_some() {
                assert!(f.verify.iter().any(|v| v.starts_with("Explain ")), "{name} {}: {:?}", f.id, f.verify);
            }
        }
        // A plan insight's key order is the one its SQL has.
        for q in &workload.queries {
            let Some(plan) = &q.plan else { continue };
            for i in plan.insights.iter().flat_map(|i| &i.items) {
                let Some((_, after)) = i.fix.split_once("the SQL below takes the filter's (") else { continue };
                let first = after.split(' ').next().unwrap_or_default();
                let sql = i.fix_sql.as_deref().unwrap_or_default();
                let keys = sql.rsplit_once('(').map(|(_, k)| k).unwrap_or_default();
                assert!(keys.trim_start().starts_with(first), "{name} {}: {} vs {sql}", q.id, i.fix);
            }
        }
    }
}

// ── verify: a verify step promises what a later scan will show ──

/// The one sentence every verify step on a source of running totals uses, and the brief says at most once.
const RUNNING_TOTALS: [&str; 4] =
    ["keeps running totals", "keeps the statements' totals", "keeps totals since", "keep totals since"];

/// Whether the finding's statements are counted in running totals (pg_stat_statements, performance_schema, the plan
/// cache): a later scan's totals still hold every run from before the change.
fn on_running_totals(report: &Report, f: &culprit_core::Finding) -> bool {
    !f.queries.is_empty() && report.snapshot.workload.as_ref().is_some_and(|w| !w.windowed)
}

#[test]
fn verify_of_statements_that_stopped_is_nothing_to_check() {
    // The demos as saved, and with every statement quiet for 100 days: then every check's findings on statements are
    // history, and none keeps its own step (which is for statements that run).
    let mut stopped = 0;
    let quiet = demo_reports_quiet_for(2400.0).into_iter().map(|(name, r)| (format!("{name} (quiet 100 days)"), r));
    for (name, report) in demo_reports().into_iter().chain(quiet) {
        for f in report.findings.iter().filter(|f| f.inactive) {
            stopped += 1;
            assert!(
                f.verify.len() == 1 && f.verify[0].starts_with("Nothing to check"),
                "{name} {}: a finding whose statements stopped has nothing to check unless they run again: {:?}",
                f.id,
                f.verify
            );
            // Nothing ships for statements that stopped: no export promises a re-scan shows it gone.
            assert!(!culprit_checks::gone_after_rescan(&report, f), "{name} {}", f.id);
            let md = culprit_engine::export::finding_markdown(&report, f, &Default::default());
            assert!(!md.contains("culprit verify"), "{name} {}: {md}", f.id);
            // Said once: the brief's codebase section doesn't say it again.
            let brief = culprit_checks::agent_brief(&report, f);
            assert_eq!(brief.to_lowercase().matches("nothing to check").count(), 1, "{name} {}:\n{brief}", f.id);
        }
    }
    assert!(stopped >= 10, "the quiet demos have findings on statements that stopped: {stopped}");
}

#[test]
fn verify_of_a_fix_that_cuts_runs_expects_fewer_runs_never_fewer_reads() {
    // A fix whose lever is how often the statement runs (a cache, a skipped or batched call, a rarer poll, the pool's
    // reset): fewer runs and less time; what one run reads stays, or rises.
    let cuts_runs = |fix: &str| {
        ["the cost is in how often it runs", "what costs is the write itself", "Most of its runs find nothing"]
            .iter()
            .any(|lever| fix.contains(lever))
    };
    let mut seen = 0;
    for (name, report) in demo_reports() {
        for f in report.findings.iter().filter(|f| !f.inactive && f.check == "heavy-query") {
            let Some(fix) = f.fix.as_ref().filter(|fix| cuts_runs(&fix.summary)) else { continue };
            let verify = f.verify.join(" ");
            assert!(
                verify.contains("fewer times") || verify.contains("less often"),
                "{name} {}: a fix that cuts runs expects fewer runs: {verify}\nfix: {}",
                f.id,
                fix.summary
            );
        }
        for f in &report.findings {
            for v in f.verify.iter().filter(|v| v.contains("fewer times") || v.contains("less often")) {
                seen += 1;
                for promise in ["reads per run should drop", "reads and time per run should drop", "pages read per run"]
                {
                    assert!(!v.contains(promise), "{name} {}: fewer runs don't read less each: {v}", f.id);
                }
            }
        }
    }
    assert!(seen > 0, "the demos have a verify step that expects fewer runs");
}

#[test]
fn verify_on_running_totals_compares_two_scans_with_what_they_show() {
    // A statement's per-run numbers or share on running totals move by a hair after a change: the step compares two
    // scans, and What changed promises only what it carries (runs, time, time per run; never reads or a share).
    let promises = ["per run should drop", "share of database time", "reads per run", "a small part of its time"];
    for (name, report) in demo_reports() {
        for f in report.findings.iter().filter(|f| !f.inactive) {
            let compared = f.verify.iter().any(|v| v.contains("keeps running totals"));
            for v in &f.verify {
                if on_running_totals(&report, f) && promises.iter().any(|p| v.contains(p)) {
                    assert!(
                        compared || v.contains("between"),
                        "{name} {}: on running totals a later scan's numbers still hold the runs from before: {v}",
                        f.id
                    );
                }
                // What it promises there is runs and time: not a statement's reads (time "toward what its reads
                // take" is time), nor its share.
                if let Some((_, after)) = v.split_once("What changed") {
                    for unshown in
                        ["reads per run", "pages read per run", "rows examined per run", "share of", "reads should"]
                    {
                        assert!(!after.contains(unshown), "{name} {}: What changed shows no {unshown}: {v}", f.id);
                    }
                }
                // Said the CLI's way too, with a scan when the change ships.
                if v.contains("What changed") && v.contains("keeps running totals") {
                    assert!(v.contains("culprit changes ") && v.contains("--since"), "{name} {}: {v}", f.id);
                    assert!(v.contains("when the change ships"), "{name} {}: {v}", f.id);
                }
            }
            // Those totals keep it listed after the right change: no export promises a re-scan shows it gone, unless
            // the finding reads a plan the change alters (it says it is gone then).
            if f.verify.iter().any(|v| v.contains("keeps running totals"))
                && !f.verify.iter().any(|v| v.contains("this finding should be gone"))
            {
                let md = culprit_engine::export::finding_markdown(&report, f, &Default::default());
                assert!(!md.contains("culprit verify"), "{name} {}: {md}", f.id);
            }
        }
    }
}

#[test]
fn verify_running_totals_said_once_per_brief() {
    for (name, report) in demo_reports() {
        for f in &report.findings {
            let brief = culprit_checks::agent_brief(&report, f);
            let said: usize = RUNNING_TOTALS.iter().map(|s| brief.matches(s).count()).sum();
            assert!(said <= 1, "{name} {}: the running-totals sentence {said} times:\n{brief}", f.id);
            assert!(!brief.contains("finding can stay until"), "{name} {}:\n{brief}", f.id);
        }
    }
}

// ── history: "stopped" said on a day of silence; history told in the past, with nothing to create ──

/// Each demo snapshot read as if its statements had all gone quiet `hours` before its data ends: every statement's
/// last run (and its plans') moved there, its span of runs kept. The demos hold no statement that stopped, so this is
/// how the rules for history meet every check that rests on statements.
fn demo_reports_quiet_for(hours: f64) -> Vec<(String, Report)> {
    crate::golden::demo_files()
        .into_iter()
        .map(|(name, path)| {
            let mut s = culprit_engine::reports::load_snapshot(&path).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            culprit_engine::plans::reparse_saved(&mut s);
            quiet_for(&mut s, hours);
            (name, culprit_engine::reading::for_reading(culprit_checks::analyze(s)))
        })
        .collect()
}

fn quiet_for(s: &mut culprit_core::Snapshot, hours: f64) {
    use culprit_core::format::{epoch_seconds, iso_from_epoch};
    let taken = s.taken_at.clone();
    // The optimizer's requests went quiet with the statements that made them.
    let end = s.workload.as_ref().and_then(|w| w.window_end.clone()).unwrap_or(taken.clone());
    if let Some(end) = epoch_seconds(&end) {
        for m in s.missing_indexes.iter_mut().filter(|m| m.last_used.is_some()) {
            m.last_used = Some(iso_from_epoch(end - hours * 3600.0));
        }
    }
    let Some(w) = s.workload.as_mut() else { return };
    let Some(end) = epoch_seconds(w.window_end.get_or_insert(taken)) else { return };
    let window = w.window_hours * 3600.0;
    let last = end - hours * 3600.0;
    let moved = |t: &Option<String>, by: f64| t.as_deref().and_then(epoch_seconds).map(|t| iso_from_epoch(t - by));
    for q in &mut w.queries {
        let was = q.last_seen.as_deref().and_then(epoch_seconds).unwrap_or(end);
        let span = q.first_seen.as_deref().and_then(epoch_seconds).map_or(window, |first| was - first);
        let by = was - last;
        q.last_seen = Some(iso_from_epoch(last));
        q.first_seen = Some(iso_from_epoch(last - span.max(0.0)));
        for p in &mut q.plans {
            p.first_seen = moved(&p.first_seen, by);
            p.last_seen = moved(&p.last_seen, by);
        }
    }
}

/// A finding's title in the present tense where its check has a past form: the verbs the checks put after their
/// subject ("An update of orders spends…", "orders is read in full…").
fn present_tense(title: &str) -> Option<&'static str> {
    const PRESENT: [&str; 14] = [
        " takes ",
        " spends ",
        " is read in full",
        " builds ",
        " averages ",
        " asks for ",
        " returns ",
        " spills ",
        " fails ",
        " examines ",
        " is searched by ",
        " waits ",
        " runs out of memory",
        " is planned without",
    ];
    if let Some(p) = PRESENT.iter().find(|p| title.contains(*p)) {
        return Some(p);
    }
    // "runs 30K times": the verb, not "some runs take".
    title
        .match_indices(" runs ")
        .find(|(at, _)| title[at + 6..].starts_with(|c: char| c.is_ascii_digit()))
        .map(|_| " runs N")
}

/// Fix SQL that creates an object for the statements: an index or statistics (`CREATE INDEX`, `CREATE STATISTICS`,
/// `ALTER TABLE … ADD`), not an event session that only watches.
fn creates_something(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    [
        "CREATE INDEX",
        "CREATE UNIQUE",
        "CREATE NONCLUSTERED",
        "CREATE CLUSTERED",
        "CREATE STATISTICS",
        " ADD INDEX",
        " ADD KEY",
    ]
    .iter()
    .any(|s| upper.contains(s))
}

/// The rule for history, on every finding of `report`: "stopped" only after a day of silence, a finding on statements
/// that stopped told in the past and offering nothing to create, an active one never told as stopped.
fn history_holds(name: &str, report: &Report, problems: &mut Vec<String>) {
    let end = report.snapshot.workload.as_ref().and_then(|w| w.window_end.clone());
    let end = end.unwrap_or(report.snapshot.taken_at.clone());
    let epoch = culprit_core::format::epoch_seconds;
    for f in &report.findings {
        let mut problem = |p: String| problems.push(format!("{name} {}: {p}", f.id));
        let texts = std::iter::once(&f.summary).chain(&f.caveats).chain(f.fix.iter().map(|x| &x.summary));
        for t in texts.filter(|t| t.contains("queries has not run")) {
            problem(format!("a double negative: {t}"));
        }
        if !f.inactive {
            if f.title.contains("until it stopped") {
                problem(format!("active, yet: {}", f.title));
            }
            continue;
        }
        let Some(last) = f.last_seen.as_deref() else {
            problem("stopped, with no last run".into());
            continue;
        };
        let silence = epoch(&end).zip(epoch(last)).map(|(e, l)| (e - l) / 3600.0);
        if !silence.is_some_and(|h| h >= culprit_core::PER_DAY_MIN_HOURS) {
            problem(format!("called stopped after {silence:?} hours of silence"));
        }
        if let Some(verb) = present_tense(&f.title) {
            problem(format!("stopped, yet its title says \"{verb}\": {}", f.title));
        }
        if let Some(sql) = f.fix.as_ref().and_then(|x| x.sql.as_deref()).filter(|sql| creates_something(sql)) {
            problem(format!("stopped, yet its fix creates something:\n{sql}"));
        }
    }
}

#[test]
fn stopped_is_said_after_a_day_of_silence_and_history_creates_nothing() {
    let mut problems = Vec::new();
    for (name, report) in demo_reports() {
        history_holds(&name, &report, &mut problems);
    }
    // Every statement quiet for 100 days: findings on statements are history, in every check that has them.
    let mut stopped = 0;
    for (name, report) in demo_reports_quiet_for(2400.0) {
        history_holds(&format!("{name} (quiet 100 days)"), &report, &mut problems);
        stopped += report.findings.iter().filter(|f| f.inactive).count();
    }
    assert!(stopped >= 10, "the quiet demos have findings on statements that stopped: {stopped}");
    // Quiet for three hours: too soon to say they stopped. Those findings stay active and say how long it has been.
    let mut quiet = 0;
    for (name, report) in demo_reports_quiet_for(3.0) {
        history_holds(&format!("{name} (quiet 3 hours)"), &report, &mut problems);
        for f in report.findings.iter().filter(|f| f.inactive) {
            problems.push(format!("{name} (quiet 3 hours) {}: stopped after three hours", f.id));
        }
        quiet +=
            report.findings.iter().flat_map(|f| &f.caveats).filter(|c| c.contains("too short a time to say")).count();
    }
    assert!(quiet > 0, "findings on statements quiet for three hours say so");
    assert!(problems.is_empty(), "{} findings break the rule for history:\n{}", problems.len(), problems.join("\n"));
}

#[test]
fn a_floor_is_said_as_one_and_a_scan_named_when_it_is_compared_with() {
    let mut deadlocks = 0;
    for (name, report) in demo_reports() {
        for f in &report.findings {
            // InnoDB keeps its latest deadlock: "at least once" in the title is "at least" beside it too.
            if f.title.contains("at least once") {
                deadlocks += 1;
                assert!(f.impact.label.starts_with("at least "), "{name} {}: {}", f.id, f.impact.label);
            }
            // "The previous scan" says which one, where a sentence compares with it.
            let texts = std::iter::once(&f.summary).chain(f.fix.iter().map(|x| &x.summary));
            for t in texts.filter(|t| t.contains("previous scan")) {
                for (at, _) in t.match_indices("previous scan") {
                    let after = &t[at..t.len().min(at + 40)];
                    assert!(after.contains(" on ") || after.contains("UTC"), "{name} {}: which scan? {t}", f.id);
                }
            }
        }
    }
    assert!(deadlocks > 0, "a demo has an InnoDB deadlock");
}

/// Each demo read again on a server started `days` before its scan, its counters that old (`uptime_hours`), and, with
/// `digests_too`, performance_schema's statement digests counting from that start as well.
fn demo_reports_up_for(days: f64, digests_too: bool) -> Vec<(String, Report)> {
    use culprit_core::format::{epoch_seconds, iso_from_epoch};
    demo_reports()
        .into_iter()
        .filter_map(|(name, report)| {
            let mut s = report.snapshot.clone();
            let taken = epoch_seconds(&s.taken_at)?;
            s.server.started_at = Some(iso_from_epoch(taken - days * 86_400.0));
            s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(days * 24.0);
            if digests_too
                && let Some(w) =
                    s.workload.as_mut().filter(|w| w.kind() == Some(culprit_core::WorkloadKind::PerformanceSchema))
            {
                w.window_start = s.server.started_at.clone();
                w.window_hours = w.window_hours.max(days * 24.0);
            }
            let label = if digests_too { "with its digests" } else { "its digests as they were" };
            Some((
                format!("{name}, up {days} days, {label}"),
                culprit_engine::reading::for_reading(culprit_checks::analyze(s)),
            ))
        })
        .collect()
}

// D (MY-5): performance_schema's table and index counters start over with its statements, and the server's uptime
// doesn't. Where the digests begin long after the server started, "never read" (and "barely read", "no deletes") wait a
// week of the digests' own counting, and the days a finding says it counted are theirs, however long the server is up.
#[test]
fn absence_on_counters_that_may_be_reset_waits_a_week_of_their_own() {
    let mut reset = 0;
    for (name, report) in demo_reports_up_for(30.0, false) {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        if w.kind() != Some(culprit_core::WorkloadKind::PerformanceSchema) || w.window_hours >= 7.0 * 24.0 {
            continue;
        }
        reset += 1;
        for f in &report.findings {
            assert!(
                !matches!(f.check.as_str(), "unused-index" | "overlapping-index"),
                "{name} {}: absence on {:.1} h of digests: {}",
                f.id,
                w.window_hours,
                f.title
            );
            for m in f.metrics.iter().filter(|m| m.label == "Counting for") {
                assert!(m.value * 24.0 <= w.window_hours + 1e-6, "{name} {}: counting for {} days", f.id, m.value);
            }
        }
        let advice = culprit_checks::index_advice(&report);
        assert!(
            advice.notes.iter().any(|n| n.contains("performance_schema's statements")),
            "{name}: {:?}",
            advice.notes
        );
    }
    assert!(reset >= 2, "the MySQL and MariaDB demos read performance_schema: {reset}");
    // The same demos with their digests as old as the server: a month of counting, and the unread indexes listed.
    let unused = demo_reports_up_for(30.0, true)
        .iter()
        .filter(|(_, r)| {
            r.snapshot
                .workload
                .as_ref()
                .is_some_and(|w| w.kind() == Some(culprit_core::WorkloadKind::PerformanceSchema))
        })
        .flat_map(|(_, r)| r.findings.iter().filter(|f| f.check == "unused-index"))
        .count();
    assert!(unused > 0, "a month of performance_schema counters lists an unread index");
}

// ── scans: a share and a rate on running totals are what Culprit's scans measured ──

/// Each demo report whose workload has a timeline of Culprit's own scans, read again with those scans stretched to
/// `factor` times their hours and a month of totals around them: a day or more of scans, which no demo fixture holds
/// (they cover a few hours), so every statement they measured is read by them, its totals saying something else.
fn demo_reports_with_scans_stretched(factor: f64) -> Vec<(String, Report)> {
    use culprit_core::format::{epoch_seconds, iso_from_epoch};
    demo_reports()
        .into_iter()
        .filter_map(|(name, report)| {
            let mut s = report.snapshot.clone();
            let w = s.workload.as_mut().filter(|w| !w.windowed)?;
            let t = w.timeline.as_mut().filter(|t| t.source == culprit_core::TimelineSource::Scans)?;
            let end = epoch_seconds(&t.end)?;
            for start in t.starts.iter_mut() {
                *start = iso_from_epoch(end - (end - epoch_seconds(start)?) * factor);
            }
            w.window_hours = w.window_hours.max(30.0 * 24.0);
            w.window_start = Some(iso_from_epoch(end - w.window_hours * 3600.0));
            Some((format!("{name}, scans stretched"), culprit_engine::reading::for_reading(culprit_checks::analyze(s))))
        })
        .collect()
}

/// [`demo_reports_with_scans_stretched`], with every statement's totals ten times what its scans counted (its runs, time,
/// rows, reads, temp files, waits): the scans hold a tenth of them, so no entry is recent and every statement the scans
/// measured has figures over the scans and figures kept only in its totals side by side.
fn demo_reports_with_long_totals() -> Vec<(String, Report)> {
    use culprit_core::format::{epoch_seconds, iso_from_epoch};
    demo_reports()
        .into_iter()
        .filter_map(|(name, report)| {
            let mut s = report.snapshot.clone();
            let w = s.workload.as_mut().filter(|w| !w.windowed)?;
            let t = w.timeline.as_mut().filter(|t| t.source == culprit_core::TimelineSource::Scans)?;
            let end = epoch_seconds(&t.end)?;
            for start in t.starts.iter_mut() {
                *start = iso_from_epoch(end - (end - epoch_seconds(start)?) * 8.0);
            }
            w.window_hours = w.window_hours.max(30.0 * 24.0);
            w.window_start = Some(iso_from_epoch(end - w.window_hours * 3600.0));
            let ten = |v: &mut Option<f64>| *v = v.map(|v| v * 10.0);
            w.total_time_ms *= 10.0;
            w.total_executions *= 10.0;
            for q in &mut w.queries {
                q.executions *= 10.0;
                q.total_time_ms *= 10.0;
                for v in
                    [&mut q.cpu_time_ms, &mut q.rows, &mut q.logical_reads, &mut q.physical_reads, &mut q.temp_writes]
                {
                    ten(v);
                }
                ten(&mut q.rows_affected);
                for wait in &mut q.waits {
                    wait.wait_ms *= 10.0;
                }
            }
            let label = format!("{name}, long totals");
            Some((label, culprit_engine::reading::for_reading(culprit_checks::analyze(s))))
        })
        .collect()
}

/// Metrics that state a statement's runs, share or average with no basis: beside figures over Culprit's scans, a reader
/// can't tell which period such a figure covers.
const BARE_STATEMENT_METRICS: &[&str] = &["Runs per day", "Share of executions", "Average duration", "Average"];
/// Metrics of what the source keeps only in a statement's totals: a part of its time, what a run spills.
const TOTALS_ONLY_METRICS: &[&str] =
    &["Slow runs' extra time", "Temp written per run", "On-disk temporary tables per run"];

/// Where a finding's statement is read over Culprit's scans, each figure says what it is over: the scans' runs, share
/// and average are named so (never a bare "Runs per day"), and what the totals alone keep (its slow runs' excess, a
/// spill, the rows a run handles) is named over the totals, so the two periods aren't read as one. Returns how many
/// findings it read.
fn bases_named(name: &str, report: &Report) -> usize {
    let Some(w) = report.snapshot.workload.as_ref() else { return 0 };
    let mut read = 0;
    for f in &report.findings {
        let Some(q) = f.queries.first().and_then(|r| w.query(&r.id)) else { continue };
        let Some(s) = w.scans_of(q).filter(|_| !w.cadence(q).is_burst()) else { continue };
        read += 1;
        for m in &f.metrics {
            assert!(!BARE_STATEMENT_METRICS.contains(&m.label.as_str()), "{name} {}: {} over what?", f.id, m.label);
        }
        if s.is_recent(q) {
            continue;
        }
        let totals = w.window_phrase();
        for m in f.metrics.iter().filter(|m| TOTALS_ONLY_METRICS.iter().any(|t| m.label.starts_with(t))) {
            assert!(m.label.contains(&totals), "{name} {}: {} isn't said over {totals}", f.id, m.label);
        }
        let summary = &f.summary;
        if summary.contains("of all statement runs") {
            assert!(
                summary.contains(&format!("of all statement runs over {}", s.phrase())),
                "{name} {}: {summary}",
                f.id
            );
        }
        if f.check == "temp-spill" || summary.contains("more than the average, together") {
            assert!(summary.contains(&totals), "{name} {}: {summary}", f.id);
        }
    }
    read
}

// Theme A's open ends. On running totals a statement's runs, share and average are what Culprit's scans measured, and
// a part of its time (slow runs' excess, a spill) is kept only in its totals: wherever both are said, each names its
// period. A stopped statement's share is "until it stopped", whatever it spent the time on.
#[test]
fn each_figure_says_what_it_is_over_beside_culprits_scans() {
    let mut read = 0;
    let readings =
        demo_reports().into_iter().chain(demo_reports_with_scans_stretched(8.0)).chain(demo_reports_with_long_totals());
    for (name, report) in readings {
        read += bases_named(&name, &report);
    }
    assert!(read >= 3, "the stretched demos' findings are read over the scans: {read}");
    let mut stopped = 0;
    for (name, report) in demo_reports_quiet_for(2400.0) {
        for f in report.findings.iter().filter(|f| f.inactive && f.check == "heavy-query" && f.title.contains("% of "))
        {
            stopped += 1;
            assert!(f.title.contains(" until it stopped"), "{name} {}: {}", f.id, f.title);
        }
    }
    assert!(stopped > 0, "the quiet demos have heavy queries that stopped");
}

/// The rule for running totals in one report: wherever a statement's share, rate and average are read (a heavy query's
/// title, summary, evidence, impact and grade; every brief's list of statements), they are the ones
/// `Workload::cost_of` states, over Culprit's scans where those cover a day, and a statement that stopped never takes
/// one of the six heavy places of one that runs. Returns how many statements it read by the scans.
fn scans_rule_holds(name: &str, report: &Report) -> usize {
    use culprit_core::format;
    let Some(w) = report.snapshot.workload.as_ref() else { return 0 };
    let mut read = 0;
    for f in &report.findings {
        let brief = culprit_checks::agent_brief(report, f);
        for line in brief.lines().filter_map(|l| l.strip_prefix("Query `")) {
            let id = line.split('`').next().unwrap_or_default();
            let Some(q) = w.query(id) else { continue };
            let cost = w.cost_of(q);
            // A run or two far apart are what they took, not an average (`QueryStat::few_runs`).
            let took = q
                .few_runs_took()
                .map_or_else(|| format!("{} average", format::duration(cost.mean_ms)), |t| format!("which took {t}"));
            let said = format!(" — {} of DB time, {}, {took}", format::percent(cost.share), cost.runs);
            assert!(
                line.contains(&said),
                "{name} {}: the brief's line for {id} isn't what it costs ({said}): {line}",
                f.id
            );
            if let Some(s) = w.scans_of(q) {
                read += 1;
                // A burst is said by when it ran, as `cost_of` says it, the scans' buckets included.
                assert!(
                    w.cadence(q).is_burst() || cost.runs.contains(&s.phrase()),
                    "{name} {}: {id}'s runs aren't the scans': {line}",
                    f.id
                );
            }
        }
        let first = f.queries.first().and_then(|r| w.query(&r.id));
        // An unstable statement's title states its average the way every surface does; a chatty one's runs too.
        if let Some(q) = first.filter(|_| f.check == "unstable-query") {
            let mean = format::duration(w.statement_mean_ms(q));
            assert!(f.title.contains(&format!(" {mean} per run")), "{name} {}: {} isn't at {mean}", f.id, f.title);
        }
        if let Some((_, s)) = first
            .filter(|q| f.check == "chatty-query" && !w.cadence(q).is_burst())
            .and_then(|q| Some((q, w.scans_of(q)?)))
        {
            assert!(f.title.contains(&s.phrase()), "{name} {}: {} isn't the scans' runs", f.id, f.title);
        }
        if f.check != "heavy-query" {
            continue;
        }
        let Some(q) = first else { continue };
        let share = w.statement_share(q);
        let impact = f.impact.db_time_share.unwrap_or(-1.0);
        assert!((impact - share).abs() < 1e-9, "{name} {}: graded on {impact}, stated at {share}", f.id);
        if f.title.contains("% of") {
            assert!(f.title.contains(&format::percent(share)), "{name} {}: {} isn't at {share}", f.id, f.title);
        }
        let Some(s) = w.scans_of(q) else { continue };
        // A burst's time is never a rate a day (`Workload::statement_time_per_day`).
        let a_day = (!w.cadence(q).is_burst()).then(|| s.per_day(s.time_ms));
        assert_eq!(f.impact.time_ms_per_day, a_day, "{name} {}: time a day", f.id);
        let lead = format!("{} of Culprit's own scans", s.length());
        assert!(f.summary.contains(&lead), "{name} {}: the summary doesn't lead with the scans: {}", f.id, f.summary);
        let metric = |label: &str| f.metrics.iter().find(|m| m.label == label).map(|m| m.value);
        assert_eq!(metric("Share of DB time over Culprit's scans"), Some(s.share()), "{name} {}", f.id);
        assert_eq!(metric("Average duration over Culprit's scans"), s.mean_ms(), "{name} {}", f.id);
        // The totals since the source began counting are given beside the scans' only where they differ twofold.
        let totals = format!("over {}", w.window_phrase());
        let beside = f.metrics.iter().filter(|m| m.label.ends_with(&totals)).count();
        // A burst's runs are when it ran, never a rate a day, on either side.
        let runs_differ = s.runs_differ(w, q) && !w.cadence(q).is_burst();
        let differs = !s.is_recent(q) && (s.share_differs(w, q) || runs_differ || s.mean_differs(q));
        assert_eq!(beside > 0, differs, "{name} {}: the totals beside the scans' {:?}", f.id, f.metrics);
    }
    // Six heavy places at most, for statements that run: history only beside them.
    let running = report.findings.iter().filter(|f| f.check == "heavy-query" && !f.inactive).count();
    assert!(running <= 6, "{name}: {running} heavy queries that run");
    read
}

#[test]
fn a_statement_on_running_totals_is_read_by_culprits_scans_everywhere() {
    for (name, report) in demo_reports() {
        scans_rule_holds(&name, &report);
    }
    let mut read = 0;
    let stretched = demo_reports_with_scans_stretched(8.0);
    assert!(stretched.len() >= 2, "PostgreSQL and MariaDB demos keep running totals with Culprit's scans");
    for (name, report) in &stretched {
        read += scans_rule_holds(name, report);
        // The headline's share is over the scans when they measured every statement behind it.
        let w = report.snapshot.workload.as_ref().expect("a workload");
        let top: Vec<&culprit_core::QueryStat> = report
            .findings
            .iter()
            .filter(|f| !f.inactive)
            .take(3)
            .flat_map(|f| f.queries.iter().filter_map(|r| w.query(&r.id)))
            .collect();
        if report.summary.headline.contains("The top ones touch") && top.iter().all(|q| w.scans_of(q).is_some()) {
            assert!(report.summary.headline.contains("of Culprit's scans"), "{name}: {}", report.summary.headline);
        }
    }
    assert!(read >= 3, "the stretched demos' statements are read by the scans: {read}");
}

// ── bursts: a burst is said, graded and judged as a burst; history is graded as history ──

/// Each demo snapshot with every statement's runs moved into one ten-minute burst, 90 to 80 minutes before its data
/// ends, read in a window of `hours` around it: the same runs and the same time, only the window longer or shorter.
/// The demos' statements run steadily over their windows (or in a window of minutes), so this is how the rule for
/// bursts meets every check that rests on statements.
fn demo_reports_in_a_burst(hours: f64) -> Vec<(String, Report)> {
    crate::golden::demo_files()
        .into_iter()
        .map(|(name, path)| {
            let mut s = culprit_engine::reports::load_snapshot(&path).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            culprit_engine::plans::reparse_saved(&mut s);
            in_a_burst(&mut s, hours);
            (name, culprit_engine::reading::for_reading(culprit_checks::analyze(s)))
        })
        .collect()
}

fn in_a_burst(s: &mut culprit_core::Snapshot, hours: f64) {
    use culprit_core::format::{epoch_seconds, iso_from_epoch};
    let taken = s.taken_at.clone();
    let Some(w) = s.workload.as_mut() else { return };
    let Some(end) = epoch_seconds(w.window_end.get_or_insert(taken)) else { return };
    w.window_hours = hours;
    w.window_start = Some(iso_from_epoch(end - hours * 3600.0));
    // Buckets would say the runs came elsewhere.
    w.timeline = None;
    let (from, to) = (Some(iso_from_epoch(end - 90.0 * 60.0)), Some(iso_from_epoch(end - 80.0 * 60.0)));
    for q in &mut w.queries {
        q.trend = None;
        (q.first_seen, q.last_seen) = (from.clone(), to.clone());
        for p in &mut q.plans {
            (p.first_seen, p.last_seen) = (from.clone(), to.clone());
        }
    }
}

/// The statements a finding rests on, where every one of them came in bursts ([`culprit_core::Cadence::Burst`]).
fn burst_statements<'a>(report: &'a Report, f: &culprit_core::Finding) -> Option<Vec<&'a culprit_core::QueryStat>> {
    let w = report.snapshot.workload.as_ref()?;
    let queries: Vec<_> = f.queries.iter().filter_map(|r| w.query(&r.id)).collect();
    (!queries.is_empty() && queries.iter().all(|q| w.cadence(q).is_burst())).then_some(queries)
}

/// A burst is said by when it ran: no rate a day in what a person reads of the finding (title, summary, impact,
/// metrics, its brief's statement lines), the time a day left unsaid.
fn burst_said_as_it_came(name: &str, report: &Report, f: &culprit_core::Finding, queries: &[&culprit_core::QueryStat]) {
    for text in [&f.title, &f.summary, &f.impact.label] {
        for a_day in ["times a day", "runs a day", "a day of", "rows a day", "writes a day"] {
            assert!(!text.contains(a_day), "{name} {}: a burst said as \"{a_day}\": {text}", f.id);
        }
    }
    for m in &f.metrics {
        assert!(!m.label.contains("per day") && !m.label.contains("a day"), "{name} {}: metric {}", f.id, m.label);
    }
    assert_eq!(f.impact.time_ms_per_day, None, "{name} {}: a burst's time spread over a day", f.id);
    let brief = culprit_checks::agent_brief(report, f);
    for q in queries {
        let Some(line) = brief.lines().find(|l| l.starts_with(&format!("Query `{}`", q.id))) else { continue };
        assert!(!line.contains(" a day"), "{name} {}: {line}", f.id);
        assert!(line.contains(" on "), "{name} {}: when it ran: {line}", f.id);
    }
}

#[test]
fn a_burst_is_said_and_graded_as_a_burst_whatever_the_window_around_it() {
    // A burst's grade is its total, never extrapolated to a day: the same runs read in a window of 2 hours (×12 a day
    // if extrapolated), 2 days or 6 days (diluted) give every finding on them the same severity, and a "busy" check
    // (chatty's 20K runs a day) finds the same statements busy in all three. The planted N+1 (60K lookups in the SQL
    // Server demo) stays busy in six days, where 60K over the window is 10K a day.
    let windows = [2.0, 48.0, 144.0];
    let readings: Vec<Vec<(String, Report)>> = windows.iter().map(|&h| demo_reports_in_a_burst(h)).collect();
    // As the demos are, and with a day of Culprit's scans: a statement whose runs all came in a bucket or two of them is
    // a burst too.
    for (name, report) in demo_reports().iter().chain(&demo_reports_with_scans_stretched(8.0)) {
        for f in &report.findings {
            if let Some(queries) = burst_statements(report, f) {
                burst_said_as_it_came(name, report, f, &queries);
            }
        }
    }
    let mut compared = 0;
    for (i, (name, report)) in readings[0].iter().enumerate() {
        let others: Vec<&Report> = readings[1..].iter().map(|r| &r[i].1).collect();
        for (h, r) in windows.iter().zip(std::iter::once(report).chain(others.iter().copied())) {
            for f in &r.findings {
                if let Some(queries) = burst_statements(r, f) {
                    burst_said_as_it_came(&format!("{name} in {h} h"), r, f, &queries);
                }
            }
        }
        for f in report.findings.iter().filter(|f| burst_statements(report, f).is_some()) {
            for (h, other) in windows[1..].iter().zip(&others) {
                let same = other.findings.iter().find(|o| o.id == f.id);
                let same = same.unwrap_or_else(|| panic!("{name}: {} in 2 h, none in {h} h: {}", f.id, f.title));
                assert_eq!(
                    f.severity, same.severity,
                    "{name} {}: {} in 2 h, {:?} in {h} h",
                    f.id, f.title, same.severity
                );
                compared += 1;
            }
        }
        for (h, other) in windows[1..].iter().zip(&others) {
            for o in other.findings.iter().filter(|o| burst_statements(other, o).is_some()) {
                assert!(
                    report.findings.iter().any(|f| f.id == o.id),
                    "{name}: {} in {h} h, none in 2 h: {}",
                    o.id,
                    o.title
                );
            }
        }
    }
    assert!(compared >= 20, "the demos' bursts make findings to compare: {compared}");
    let chatty = |r: &[(String, Report)]| {
        r.iter().find(|(n, _)| n == "mssql-demo").map(|(_, r)| r.findings.iter().any(|f| f.id == "chatty-query:6"))
    };
    assert_eq!(chatty(&readings[2]), Some(true), "the N+1 is busy at the pace it ran");
}

#[test]
fn a_finding_on_statements_that_stopped_is_low() {
    // History, graded as a single run is: whatever the check, its cost is behind it.
    let mut stopped = 0;
    for (name, report) in demo_reports().into_iter().chain(demo_reports_quiet_for(2400.0)) {
        for f in report.findings.iter().filter(|f| f.inactive) {
            stopped += 1;
            assert_eq!(
                f.severity,
                culprit_core::Severity::Low,
                "{name} {}: stopped, yet {:?}: {}",
                f.id,
                f.severity,
                f.title
            );
        }
    }
    assert!(stopped >= 10, "the quiet demos have findings on statements that stopped: {stopped}");
}

#[test]
fn one_run_or_two_far_apart_are_said_as_what_they_took() {
    // An average over a run or two hides what happened ("5.2 s per run" for 10.5 s and 0.2 ms): every brief line and
    // export of such a statement, and a lock-time title, say what the runs took instead.
    let mut seen = 0;
    for (name, report) in demo_reports().into_iter().chain(demo_reports_in_a_burst(48.0)) {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        for f in &report.findings {
            let brief = culprit_checks::agent_brief(&report, f);
            for q in f.queries.iter().filter_map(|r| w.query(&r.id)).filter(|q| q.few_runs().is_some()) {
                seen += 1;
                let line = brief.lines().find(|l| l.starts_with(&format!("Query `{}`", q.id)));
                let line = line.unwrap_or_else(|| panic!("{name} {}: no line for {}", f.id, q.id));
                assert!(!line.contains("average") && line.contains("which took"), "{name} {}: {line}", f.id);
                let md = culprit_engine::export::finding_markdown(&report, f, &Default::default());
                assert!(md.lines().filter(|l| l.contains(&q.id)).all(|l| !l.contains("on average")), "{name} {}", f.id);
                if f.check == "lock-time" {
                    assert!(!f.title.contains("per run"), "{name} {}: {}", f.id, f.title);
                }
            }
        }
    }
    assert!(seen > 0, "the demos have statements that ran once or twice");
}

// ── plans: a plan reading states only what the plan supports ──

/// Each demo report as it reads when the server's list of missing-index requests is empty (a restart empties it, as on
/// a freshly started SQL Server): the plans' own requests are all that is left of them.
fn demo_reports_without_index_requests() -> Vec<(String, Report)> {
    demo_reports()
        .into_iter()
        .filter(|(_, r)| !r.snapshot.missing_indexes.is_empty())
        .map(|(name, report)| {
            let mut s = report.snapshot.clone();
            s.missing_indexes.clear();
            (format!("{name}, no index requests"), culprit_engine::reading::for_reading(culprit_checks::analyze(s)))
        })
        .collect()
}

/// Whether the statement's runs are as cheap as its plan is costed, or can't say: only then is a plan "cheap".
fn cheap_as_measured(plan: &culprit_core::Plan, q: &culprit_core::QueryStat) -> bool {
    use culprit_core::insights::{CheapPlanRuns, cheap_plan_runs};
    matches!(cheap_plan_runs(plan, Some(q)), CheapPlanRuns::Cheap | CheapPlanRuns::Unmeasured)
}

/// Where a step's share is stated, on every surface that states one (a brief's Plan line, a finding's export, a plan's
/// export and its insights): the plan does real work, and the step's cost doesn't rest on a guess (its own, one below
/// it, or a loop's over one). Where none is, the reason given is the true one: a plan whose costs rest on a guess, or
/// whose runs far exceed its estimate or read a whole table, is never called cheap.
fn plan_rule_holds(name: &str, report: &Report) -> usize {
    use culprit_core::insights::does_real_work;
    let Some(w) = report.snapshot.workload.as_ref() else { return 0 };
    let mut read = 0;
    for q in &w.queries {
        let Some(plan) = &q.plan else { continue };
        read += 1;
        let guessed: Vec<u32> = plan.guesses().into_iter().map(|(id, _)| id).collect();
        for n in plan.nodes() {
            if plan.step_share(n).is_some() {
                assert!(does_real_work(plan) && !guessed.contains(&n.id), "{name} {}: a share for step {}", q.id, n.id);
            }
        }
        for i in plan.insights.iter().flat_map(|i| &i.items) {
            let says = format!("{} {}", i.title, i.detail).contains("of this plan's work");
            let may = plan.states_shares() && !i.nodes.iter().any(|id| guessed.contains(id));
            assert!(!says || may, "{name} {}: insight {} states a share it can't: {}", q.id, i.rule, i.detail);
            assert!(culprit_core::insights::stated_share(plan, i).is_none() || may, "{name} {} {}", q.id, i.rule);
        }
        if let Some(note) = culprit_checks::plan_shares_note(plan, Some(q))
            && note.contains("is cheap")
        {
            assert!(!plan.has_guess() && cheap_as_measured(plan, q), "{name} {}: {note}", q.id);
        }
        // The plan's own export: no share on any step of a plan that states none.
        let md = culprit_engine::export::plan_markdown(plan, "plan", &Default::default());
        if !plan.states_shares() {
            assert!(!md.contains("Where the cost goes"), "{name} {}: the export puts a share on a step", q.id);
            let costs = md
                .lines()
                .filter(|l| l.starts_with("| ") && !l.starts_with("| Step"))
                .filter_map(|l| l.split(" | ").nth(2).map(str::trim));
            for cost in costs {
                assert!(!cost.ends_with('%'), "{name} {}: a cost share in the plan's export: {cost}", q.id);
            }
        }
    }
    for f in &report.findings {
        let brief = culprit_checks::agent_brief(report, f);
        for r in &f.queries {
            let Some(q) = w.query(&r.id) else { continue };
            let Some(plan) = &q.plan else { continue };
            let Some(node) = culprit_checks::finding_step(f, r, plan) else { continue };
            let Some(at) = brief.find(&format!("Query `{}`", q.id)) else { continue };
            let rest = &brief[at..];
            let rest = &rest[..rest[1..].find("Query `").map_or(rest.len(), |i| i + 1)];
            let Some(line) = rest.lines().find(|l| l.starts_with("Plan:")) else { continue };
            if line.contains("of estimated cost)") {
                assert!(does_real_work(plan) && plan.guess(node).is_none(), "{name} {}: {line}", f.id);
            }
            if line.contains("is cheap") {
                assert!(!plan.has_guess() && cheap_as_measured(plan, q), "{name} {}: {line}", f.id);
            }
        }
        let md = culprit_engine::export::finding_markdown(report, f, &Default::default());
        if md.contains("of the plan's cost") {
            let stated = f
                .queries
                .iter()
                .filter_map(|r| Some((r, w.query(&r.id)?.plan.as_ref()?)))
                .any(|(r, p)| culprit_checks::finding_step(f, r, p).is_some_and(|n| p.step_share(n).is_some()));
            assert!(stated, "{name} {}: the export states a share its plan can't", f.id);
        }
    }
    read
}

#[test]
fn a_plan_reading_states_a_share_only_where_the_plan_supports_one() {
    let read: usize = demo_reports().iter().map(|(name, report)| plan_rule_holds(name, report)).sum();
    assert!(read > 0, "the demos have plans");
}

// A cheap plan's note says a whole-table read only where it can weigh on a run: a table of `SCAN_ROWS` rows or more (or
// that many read over the step's runs), not the engine's catalog. "… but `Seq Scan` reads all 2 rows of
// pg_catalog.pg_database on every run" flags a trivial read; there the note is the plain cheap-plan one. Every surface
// (the plan's note, a step's reason, a brief's Plan line, the exports) says it from the same place.
#[test]
fn a_whole_table_read_is_said_only_where_it_can_matter() {
    use culprit_core::insights::{does_real_work, whole_read_matters};
    let mut trivial = 0;
    let mut notes = 0;
    for (name, report) in demo_reports() {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        for q in &w.queries {
            let Some(plan) = &q.plan else { continue };
            let matters: Vec<&culprit_core::PlanNode> =
                plan.nodes().into_iter().filter(|n| whole_read_matters(plan, n)).collect();
            if plan.weighed() && !does_real_work(plan) && matters.is_empty() {
                trivial += plan.nodes().iter().any(|n| n.reads_table_whole()) as usize;
            }
            let mut said: Vec<String> =
                [Some(q), None].into_iter().filter_map(|with| culprit_checks::plan_shares_note(plan, with)).collect();
            said.extend(plan.nodes().into_iter().filter_map(|n| culprit_checks::no_share_reason(plan, n, Some(q))));
            said.push(culprit_engine::export::plan_markdown(plan, "plan", &Default::default()));
            for text in said.iter().filter(|t| t.contains(" on every run") && t.contains(" reads all ")) {
                notes += 1;
                let named =
                    matters.iter().any(|n| n.object.as_deref().is_some_and(|o| text.contains(&format!(" of {o} "))));
                assert!(named, "{name} {}: a whole-table read said that can't matter: {text}", q.id);
            }
        }
        for f in &report.findings {
            let brief = culprit_checks::agent_brief(&report, f);
            let md = culprit_engine::export::finding_markdown(&report, f, &Default::default());
            for text in [brief, md] {
                for line in
                    text.lines().filter(|l| l.contains(" on every run") && l.contains("is too small for a share"))
                {
                    let ok = f.queries.iter().filter_map(|r| w.query(&r.id)?.plan.as_ref()).any(|p| {
                        p.nodes().into_iter().any(|n| {
                            whole_read_matters(p, n)
                                && n.object.as_deref().is_some_and(|o| line.contains(&format!(" of {o} ")))
                        })
                    });
                    assert!(ok, "{name} {}: a whole-table read said that can't matter: {line}", f.id);
                }
            }
        }
    }
    // The SQL Server demo's Query Store plan reads all of a `sys.` table; the PostgreSQL demos read small tables whole.
    assert!(trivial > 0, "the demos have cheap plans that read a small or a catalog table whole: {trivial} ({notes})");
}

// MS-1: a plan's own missing-index request is a finding (`plan-insights:missing-index-hint:*`) unless a missing-index or
// filtered-scan finding is about the same table and leads with the same key; the planted shipped_at index is never
// hidden because the server's list of requests was empty.
#[test]
fn every_plan_index_request_is_a_finding_for_its_table_and_key() {
    let mut seen = 0;
    for (name, report) in demo_reports().into_iter().chain(demo_reports_without_index_requests()) {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        let total = w.total_time_ms.max(1.0);
        for q in &w.queries {
            let Some(plan) = &q.plan else { continue };
            // The requests the plan-insights check weighs: a statement that matters, a request that isn't Low.
            let weighed = plan
                .insights
                .iter()
                .flat_map(|i| &i.items)
                .any(|i| i.rule == "missing-index-hint" && i.severity != culprit_core::Severity::Low);
            if !weighed || q.total_time_ms / total < 0.02 || culprit_core::sql::is_session_statement(&q.text) {
                continue;
            }
            for hint in &plan.index_hints {
                seen += 1;
                let normal = |s: &str| s.replace(['[', ']', '"'], "").to_ascii_lowercase();
                let lead = hint.equality.iter().chain(&hint.inequality).next().map(|k| normal(k));
                let leaf = |s: &str| normal(s).rsplit('.').next().unwrap_or_default().to_string();
                let covered = report.findings.iter().any(|f| {
                    let keys = f.id.rsplit_once('(').map(|(_, k)| k.trim_end_matches(')').to_ascii_lowercase());
                    let first = keys.as_deref().and_then(|k| k.split(',').next()).map(str::trim).map(str::to_string);
                    let same_table = leaf(&f.subject.name) == leaf(&hint.table);
                    (matches!(f.check.as_str(), "missing-index" | "filtered-scan") && same_table && first == lead)
                        || (f.id.starts_with("plan-insights:missing-index-hint:")
                            && same_table
                            && f.queries.iter().any(|r| r.id == q.id))
                });
                assert!(covered, "{name} {}: the plan asks for an index on {} that no finding shows", q.id, hint.table);
            }
        }
    }
    assert!(seen > 0, "the demos have plans that ask for an index");
}

// MS-6: PostgreSQL's note on a scan planned on a default guess says a real value may well use an index only where one
// of the table's indexes has a column the scan's filter names.
#[test]
fn a_plan_note_never_promises_an_index_that_does_not_exist() {
    for (name, report) in demo_reports() {
        let s = &report.snapshot;
        for plan in s.queries().iter().filter_map(|q| q.plan.as_ref()) {
            for n in plan.nodes() {
                let Some(note) = n.props.get(culprit_core::GUESSED_SELECTIVITY) else { continue };
                if !note.contains("use an index") {
                    continue;
                }
                let filter = n.predicate.as_deref().unwrap_or_default().to_ascii_lowercase();
                let table = n.object.as_deref().unwrap_or_default();
                let indexed = s
                    .indexes
                    .iter()
                    .filter(|i| format!("{}.{}", i.schema, i.table) == table)
                    .any(|i| i.columns.iter().any(|c| filter.contains(&c.to_ascii_lowercase())));
                assert!(indexed, "{name}: {table} has no index on what `{filter}` compares, yet: {note}");
            }
        }
    }
}

// MS-7: a saved PostgreSQL plan read again takes each scanned table's size from the scan's own catalog where the plan
// carries none, so a scan of a whole table reads as one.
#[test]
fn a_saved_plan_read_again_knows_its_tables_sizes() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let s = &report.snapshot;
        if s.server.engine != culprit_core::Engine::Postgres {
            continue;
        }
        for plan in s.queries().iter().filter_map(|q| q.plan.as_ref()) {
            for n in plan.nodes().into_iter().filter(|n| n.op.contains("Seq Scan")) {
                let Some((schema, table)) = n.object.as_deref().and_then(|o| o.rsplit_once('.')) else { continue };
                let Some(rows) = s.table(schema, table).and_then(|t| t.rows).filter(|r| *r > 0.0) else { continue };
                seen += 1;
                assert_eq!(n.table_rows, Some(rows), "{name}: the scan of {schema}.{table} knows no size");
            }
        }
    }
    assert!(seen > 0, "the PostgreSQL demos scan tables the catalog has");
}

// MY-11: a scan filtered-scan names is one its plan's own rules speak about too, so the plan viewer is never silent on
// the step the finding points at.
#[test]
fn a_scan_a_finding_names_is_one_its_plan_speaks_about() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        for f in report.findings.iter().filter(|f| f.check == "filtered-scan" && !f.inactive) {
            for r in &f.queries {
                let (Some(q), Some(id)) = (w.query(&r.id), r.plan_node) else { continue };
                let Some(plan) = &q.plan else { continue };
                seen += 1;
                let spoken = plan.insights.iter().flat_map(|i| &i.items).any(|i| i.nodes.contains(&id));
                assert!(spoken, "{name} {}: plan {} says nothing about step {id}", f.id, q.id);
            }
        }
    }
    assert!(seen > 0, "the demos have scans an index would avoid");
}

// ── queries: each check reads the statement right ──

/// The statements a finding lists, as the report's workload has them.
fn listed<'a>(report: &'a Report, f: &culprit_core::Finding) -> Vec<&'a culprit_core::QueryStat> {
    let Some(w) = report.snapshot.workload.as_ref() else { return Vec::new() };
    f.queries.iter().filter_map(|r| w.query(&r.id)).collect()
}

/// Every text a person reads of a finding: title, summary, fix, its SQL and cost, verify steps and caveats.
fn finding_text(f: &culprit_core::Finding) -> String {
    let fix = f.fix.as_ref();
    [
        Some(f.title.clone()),
        Some(f.summary.clone()),
        fix.map(|x| x.summary.clone()),
        fix.and_then(|x| x.sql.clone()),
        fix.and_then(|x| x.cost.clone()),
        Some(f.verify.join(" ")),
        Some(f.caveats.join(" ")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

// MS-10, US-9: Live shows a lock's holder only while someone waits for it. A fix that sends the reader there to find
// one is for a statement that still runs, and runs at least once an hour over the window: rarer waits are logged
// next time (`support::find_lock_holder`, the one wording every check that names a lock wait uses).
#[test]
fn a_lock_holder_is_found_live_only_where_its_waits_come_often() {
    const LIVE: [&str; 4] = [
        "(Live shows the blocking chain)",
        "Live shows who blocks whom",
        "(Live shows blockers)",
        "find the holder in Live",
    ];
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        for f in &report.findings {
            let Some(fix) = f.fix.as_ref() else { continue };
            let statements = listed(&report, f);
            if statements.is_empty() || !LIVE.iter().any(|l| fix.summary.contains(l)) {
                continue;
            }
            seen += 1;
            if f.inactive {
                assert!(
                    fix.summary.contains("if it comes back"),
                    "{name} {}: Live for a statement that stopped: {}",
                    f.id,
                    fix.summary
                );
                continue;
            }
            if w.window_hours <= 0.0 {
                continue;
            }
            for q in statements {
                assert!(
                    q.executions.ceil() / w.window_hours >= 1.0,
                    "{name} {}: query {} ran {} times in {} h, too seldom to catch live: {}",
                    f.id,
                    q.id,
                    q.executions,
                    w.window_hours,
                    fix.summary
                );
            }
        }
    }
    assert!(seen > 0, "the demos have lock waits whose holder is to be found");
}

// MS-12a: statements that waited on locks on one table in the same hours waited in one episode: one lock-time finding
// lists them all.
#[test]
fn one_lock_episode_on_a_table_is_one_finding() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let s = &report.snapshot;
        let locks: Vec<(&culprit_core::Finding, &culprit_core::QueryStat)> = report
            .findings
            .iter()
            .filter(|f| f.check == "lock-time")
            .filter_map(|f| Some((f, *listed(&report, f).first()?)))
            .collect();
        seen += locks.len();
        let span = |q: &culprit_core::QueryStat| {
            Some((
                culprit_core::format::epoch_seconds(q.first_seen.as_deref()?)?,
                culprit_core::format::epoch_seconds(q.last_seen.as_deref()?)?,
            ))
        };
        let names = |q: &culprit_core::QueryStat, table: &str| {
            let text = q.text.to_ascii_lowercase().replace(['[', ']', '`', '"'], "");
            text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| w == table)
        };
        for (i, (f, a)) in locks.iter().enumerate() {
            for (g, b) in &locks[i + 1..] {
                let (Some((a1, a2)), Some((b1, b2))) = (span(a), span(b)) else { continue };
                let shared = s.tables.iter().find(|t| {
                    let t = t.name.to_ascii_lowercase();
                    names(a, &t) && names(b, &t)
                });
                assert!(
                    !(a1 <= b2 && b1 <= a2) || shared.is_none(),
                    "{name}: {} and {} waited on {} in the same hours, as two findings",
                    f.id,
                    g.id,
                    shared.map(|t| t.name.as_str()).unwrap_or_default()
                );
            }
        }
    }
    assert!(seen > 0, "the demos have lock-time findings");
}

// CX-13: a fix that forces a plan by hand says what SQL Server's automatic plan correction does where it is on, and
// asks to turn it on only where it is off.
#[test]
fn a_plan_forced_by_hand_says_what_automatic_correction_does() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let on = report.snapshot.tuning.as_ref().is_some_and(|t| {
            t.automatic_tuning
                .iter()
                .any(|o| o.name.eq_ignore_ascii_case("FORCE_LAST_GOOD_PLAN") && o.actual.eq_ignore_ascii_case("ON"))
        });
        for f in &report.findings {
            let Some(fix) = f.fix.as_ref() else { continue };
            let forces = fix.sql.as_deref().is_some_and(|sql| {
                sql.lines().any(|l| !l.trim_start().starts_with("--") && l.contains("sp_query_store_force_plan"))
            });
            if !forces {
                continue;
            }
            seen += 1;
            let text = format!("{} {}", fix.summary, fix.cost.as_deref().unwrap_or_default());
            assert_eq!(
                text.contains("Automatic plan correction (FORCE_LAST_GOOD_PLAN) is on"),
                on,
                "{name} {}: {text}",
                f.id
            );
            assert!(!(on && text.contains("turn on automatic plan correction")), "{name} {}: {text}", f.id);
        }
    }
    // The demo fixtures force no plan today (the fresh SQL Server demo and real readings do); the rule holds wherever
    // one does, and query_checks' tests build the case.
    let _ = seen;
}

// MS-12b: one answer between the plan notes and the server's settings. Where config:cost-threshold-for-parallelism
// raises the threshold to 50, no plan costed under 50 is told to drop what keeps it on one CPU: at 50 it stays there.
#[test]
fn parallelism_notes_agree_with_the_cost_threshold_advice() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        if !report.findings.iter().any(|f| f.id == "config:cost-threshold-for-parallelism") {
            continue;
        }
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        for q in &w.queries {
            let Some(plan) = &q.plan else { continue };
            seen += 1;
            let blocked = culprit_core::insights::insights(plan).into_iter().any(|i| i.rule == "parallelism-blocked");
            assert!(
                !blocked || plan.total_cost >= 50.0,
                "{name}: query {}'s plan, costed {:.0}, is told to go parallel beside the advice to raise the threshold to 50",
                q.id,
                plan.total_cost
            );
        }
    }
    assert!(seen > 0, "the demos advise the cost threshold");
}

// CX-4: stale statistics count changes, not rows; "no query has used it" is never said, and "No update is needed"
// only where no plan of a statement still running was compiled with them, its brief saying the upkeep is optional.
#[test]
fn stale_statistics_say_what_uses_them_and_count_changes() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let Some(w) = report.snapshot.workload.as_ref() else { continue };
        for f in report.findings.iter().filter(|f| f.check == "stale-stats") {
            seen += 1;
            let text = finding_text(f);
            assert!(
                !text.contains("no query has used it") && !text.contains("rows changed"),
                "{name} {}: {text}",
                f.id
            );
            assert!(!f.metrics.iter().any(|m| m.label.starts_with("Rows changed")), "{name} {}: {:?}", f.id, f.metrics);
            let Some(fix) = f.fix.as_ref().filter(|x| x.summary.starts_with("No update is needed")) else { continue };
            let names: Vec<String> = f
                .metrics
                .iter()
                .filter(|m| m.label == "Statistics")
                .filter_map(|m| m.text.clone())
                .flat_map(|t| t.split([',', ' ']).map(str::to_string).collect::<Vec<_>>())
                .filter(|n| n.len() > 3)
                .collect();
            for q in w.queries.iter().filter(|q| q.plan.is_some()) {
                let raw = q.plan.as_ref().and_then(|p| p.raw.as_deref()).unwrap_or_default();
                assert!(
                    !names.iter().any(|n| raw.contains(&format!("Statistics=\"[{n}]\""))),
                    "{name} {}: \"{}\" while query {}'s plan was compiled with them",
                    f.id,
                    fix.summary,
                    q.id
                );
            }
            let brief = culprit_checks::agent_brief(&report, f);
            assert!(brief.contains("The fix's upkeep is optional"), "{name} {}: {brief}", f.id);
        }
    }
    let _ = seen;
}

// MS-4: unstable-query reads the waits the engine recorded for the statement: no "check the waits", and no lock waits
// suggested where Query Store recorded waits and none on locks; RECOMPILE comes first only where compiling every run
// costs less than the slow runs did.
#[test]
fn unstable_queries_read_the_waits_and_rank_recompile_by_its_cost() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        let mssql = report.snapshot.server.engine == culprit_core::Engine::Mssql;
        for f in report.findings.iter().filter(|f| f.check == "unstable-query" || f.check == "parameter-sensitive") {
            seen += 1;
            assert!(!f.summary.contains("Check the waits for this query"), "{name} {}: {}", f.id, f.summary);
            for q in listed(&report, f) {
                if mssql && !q.waits.is_empty() && !q.waits.iter().any(|w| w.category == "Lock") {
                    assert!(!f.summary.contains("or some runs wait on locks"), "{name} {}: {}", f.id, f.summary);
                }
                let extra = f.metrics.iter().find(|m| m.label.starts_with("Slow runs' extra time")).map(|m| m.value);
                let compile = q.costs.as_ref().and_then(|c| Some(c.plan_ms? / c.plans.filter(|n| *n > 0.0)?));
                let first = f
                    .fix
                    .as_ref()
                    .and_then(|x| x.sql.as_deref())
                    .is_some_and(|sql| sql.contains("-- 1. Plan each call for its own values"));
                if let (Some(extra), Some(compile), true) = (extra, compile, first) {
                    assert!(
                        compile * q.executions < extra,
                        "{name} {}: RECOMPILE first at {compile} ms a compile",
                        f.id
                    );
                }
            }
        }
    }
    // The demo fixtures have no unstable statement today (the fresh SQL Server demo does); unstable_query's tests build
    // the case.
    let _ = seen;
}

// CX-10b, MY-9a: a chatty finding's share is of all statement runs, and "one row" is said only of runs that handle
// about one; a read in a loop is one key a call.
#[test]
fn chatty_findings_say_what_was_counted() {
    let mut seen = 0;
    for (name, report) in demo_reports() {
        for f in &report.findings {
            let text = finding_text(f);
            assert!(!text.contains("of every statement the database executes"), "{name} {}: {text}", f.id);
            if f.check != "chatty-query" {
                continue;
            }
            seen += 1;
            assert!(!f.title.contains("one row at a time"), "{name} {}: {}", f.id, f.title);
            if f.title.contains("one row each time") {
                for q in listed(&report, f) {
                    let rows = q.rows_affected.or(q.rows).map(|r| r / q.executions.max(1.0)).unwrap_or(1.0);
                    assert_eq!(
                        culprit_core::format::count(rows),
                        "1",
                        "{name} {}: {} with {rows} rows a run",
                        f.id,
                        f.title
                    );
                }
            }
        }
    }
    assert!(seen > 0, "the demos have chatty statements");
}

// CX-2: a table hint names what it forces (a bare FORCESEEK allows the seek the plan already does), and a fix whose
// SQL offers a change in the application as its fallback is never briefed as upkeep alone.
#[test]
fn a_hint_names_its_index_and_a_fallback_in_the_code_is_briefed_as_one() {
    for (name, report) in demo_reports() {
        for f in &report.findings {
            let text = finding_text(f);
            for (at, _) in text.match_indices("FORCESEEK") {
                let rest = text[at + "FORCESEEK".len()..].trim_start();
                assert!(rest.starts_with('(') && rest[1..].contains(" ("), "{name} {}: a bare FORCESEEK: {text}", f.id);
            }
            let offers_code = f.fix.as_ref().and_then(|x| x.sql.as_deref()).is_some_and(|sql| {
                sql.lines()
                    .any(|l| l.trim_start().starts_with("--") && l.to_ascii_lowercase().contains("in the application"))
            });
            if offers_code {
                let brief = culprit_checks::agent_brief(&report, f);
                assert!(
                    !brief.contains("The fix is upkeep of the database, not a change to the application's code"),
                    "{name} {}: {brief}",
                    f.id
                );
            }
        }
    }
}

// US-4: an INSERT … ON CONFLICT … DO UPDATE rewrites the rows it finds: its rows are said as inserted or updated.
#[test]
fn an_upsert_is_said_to_insert_or_update() {
    for (name, report) in demo_reports() {
        for f in report.findings.iter().filter(|f| f.check == "heavy-query") {
            let upsert = listed(&report, f).iter().any(|q| {
                let t = q.text.to_ascii_lowercase();
                t.contains("on conflict") && t.contains("do update")
            });
            if upsert {
                assert!(!f.summary.contains("Each run inserts "), "{name} {}: {}", f.id, f.summary);
            }
        }
    }
}

// MY-10: a fix that turns MariaDB's slow query log on into a table says what that does to lock time.
#[test]
fn a_mariadb_slow_log_in_a_table_says_what_it_does_to_lock_time() {
    for (name, report) in demo_reports() {
        if !report.snapshot.server.is_mariadb() {
            continue;
        }
        for f in &report.findings {
            let Some(fix) = f.fix.as_ref() else { continue };
            let to_table = fix.sql.as_deref().is_some_and(|sql| {
                let sql = sql.to_ascii_uppercase();
                sql.contains("SLOW_QUERY_LOG") && sql.contains("LOG_OUTPUT") && sql.contains("TABLE")
            }) || (fix.summary.contains("slow_query_log") && fix.summary.contains("TABLE"));
            if to_table {
                let text = format!("{} {}", fix.summary, fix.cost.as_deref().unwrap_or_default());
                assert!(text.contains("lock time"), "{name} {}: {text}", f.id);
            }
        }
    }
}

// CX-9: an index recommendation left on a condition ("if the queries … are slow all the same") is one the scan can't
// answer: some statement behind it runs slow, or none is listed.
#[test]
fn an_index_advice_is_conditional_only_where_a_statement_is_slow() {
    for (name, report) in demo_reports() {
        for f in report.findings.iter().filter(|f| f.id.starts_with("tuning-advice:create-index:")) {
            let conditional = f.fix.as_ref().is_some_and(|x| x.summary.contains("slow all the same"));
            let statements = listed(&report, f);
            if conditional && !statements.is_empty() {
                assert!(
                    statements.iter().any(|q| q.mean_time_ms >= 10.0),
                    "{name} {}: conditional on slow reads that all run fast",
                    f.id
                );
            }
        }
    }
}
