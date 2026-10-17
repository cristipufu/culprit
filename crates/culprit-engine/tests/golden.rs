//! What a person reads in each demo fixture (`culprit_engine::reading`: findings, briefs, plan insights), compared
//! with `fixtures/golden/<name>.txt`. A fix that changes what users read elsewhere (a finding gone, another called
//! "stopped", a title in another tense) fails here with the first lines that changed; after reading them, accept the
//! change with `CULPRIT_UPDATE_GOLDEN=1`, which rewrites the files, and commit them with it.

use std::path::{Path, PathBuf};

const UPDATE: &str = "CULPRIT_UPDATE_GOLDEN";
const ACCEPT: &str = "CULPRIT_UPDATE_GOLDEN=1 cargo test -p culprit-engine --test all golden::";
/// The four demo snapshots; every saved demo report (`*.report.json`) is read too.
const SNAPSHOTS: &[&str] = &["mssql-demo.json", "postgres-demo.json", "mysql-demo.json", "sqlite-demo.json"];
/// Changed lines shown on a mismatch, before "and N more".
const SHOWN: usize = 40;
const CONTEXT: usize = 3;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Every demo fixture, by name: its file name without `.json`.
pub(crate) fn demo_files() -> Vec<(String, PathBuf)> {
    let dir = fixtures();
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .expect("fixtures/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| {
            let file = p.file_name()?.to_str()?.to_string();
            let wanted = file.ends_with(".report.json") || SNAPSHOTS.contains(&file.as_str());
            wanted.then(|| (file.trim_end_matches(".json").to_string(), p))
        })
        .collect();
    files.sort();
    assert!(files.len() > SNAPSHOTS.len(), "the demo fixtures are missing: {files:?}");
    files
}

fn reading(path: &Path) -> String {
    let report = culprit_engine::reading::open(path).unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
    culprit_engine::reading::text(&report)
}

/// Each fixture read on its own thread: they are independent and the reads are the slow part.
fn readings(files: &[(String, PathBuf)], times: usize) -> Vec<(String, Vec<String>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .iter()
            .map(|(name, path)| scope.spawn(move || (name.clone(), (0..times).map(|_| reading(path)).collect())))
            .collect();
        handles.into_iter().map(|h| h.join().expect("a reading")).collect()
    })
}

#[test]
fn what_users_read_matches_the_golden_files() {
    let golden = fixtures().join("golden");
    let update = std::env::var(UPDATE).is_ok_and(|v| !v.is_empty() && v != "0");
    let files = demo_files();
    let mut failures = Vec::new();
    // Read twice: a reading that depends on hash order or the clock would differ between the two.
    for (name, texts) in readings(&files, 2) {
        let text = &texts[0];
        if texts[1] != *text {
            failures.push(format!(
                "{name}: two readings of the same file differ, so the golden file can't hold it\n{}",
                short_diff(text, &texts[1])
            ));
            continue;
        }
        let path = golden.join(format!("{name}.txt"));
        let accepted = std::fs::read_to_string(&path).ok();
        if accepted.as_deref() == Some(text.as_str()) {
            continue;
        }
        if update {
            std::fs::create_dir_all(&golden).expect("fixtures/golden/");
            std::fs::write(&path, text).expect("write a golden file");
            eprintln!("golden: wrote fixtures/golden/{name}.txt");
            continue;
        }
        failures.push(match accepted {
            Some(accepted) => {
                format!(
                    "fixtures/golden/{name}.txt: what users read changed{}\n{}",
                    scale(&accepted, text),
                    short_diff(&accepted, text)
                )
            }
            None => format!("fixtures/golden/{name}.txt doesn't exist yet"),
        });
    }
    // A golden file no fixture produces any more.
    let names: Vec<String> = files.iter().map(|(n, _)| format!("{n}.txt")).collect();
    for entry in std::fs::read_dir(&golden).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.ends_with(".txt") && !names.contains(&file) {
            if update {
                std::fs::remove_file(entry.path()).expect("remove a stale golden file");
                eprintln!("golden: removed fixtures/golden/{file}");
            } else {
                failures.push(format!("fixtures/golden/{file}: no demo fixture reads as it any more"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}\n\nIf every change above is intended, accept it with `{ACCEPT}` and commit fixtures/golden/ with the change.",
        failures.join("\n\n")
    );
}

/// Each `=== <kind> <id>` section of a reading: its heading and its lines.
fn sections(text: &str) -> Vec<(&str, Vec<&str>)> {
    let mut out: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        match line.strip_prefix("=== ") {
            Some(heading) => out.push((heading, Vec::new())),
            None => {
                if let Some((_, lines)) = out.last_mut() {
                    lines.push(line);
                }
            }
        }
    }
    out
}

/// The scale of a change, before its diff: the findings that appeared, disappeared or read differently, and the plans
/// whose insights or notes did.
fn scale(old: &str, new: &str) -> String {
    let (a, b) = (sections(old), sections(new));
    let findings = |list: &[(&str, Vec<&str>)]| list.iter().filter(|(h, _)| h.starts_with("finding ")).count();
    let mut parts = Vec::new();
    let appeared = missing_from(&b, &a, "finding ");
    let gone = missing_from(&a, &b, "finding ");
    let changed: Vec<&str> = a
        .iter()
        .filter(|(h, lines)| h.starts_with("finding ") && body(&b, h).is_some_and(|l| l != lines))
        .map(|(h, _)| *h)
        .collect();
    let plans = a.iter().filter(|(h, lines)| h.starts_with("plan ") && body(&b, h).is_some_and(|l| l != lines)).count()
        + missing_from(&a, &b, "plan ").len()
        + missing_from(&b, &a, "plan ").len();
    for (list, what) in [(appeared, "appeared"), (gone, "disappeared"), (changed, "read differently")] {
        if !list.is_empty() {
            parts.push(format!("{} {what}: {}", list.len(), ids(&list)));
        }
    }
    if plans > 0 {
        parts.push(format!("{plans} plan(s) read differently"));
    }
    let header = |text: &str| text.lines().take_while(|l| !l.starts_with("=== ")).collect::<Vec<_>>().join("\n");
    if header(old) != header(new) {
        parts.push("the headline or counts changed".to_string());
    }
    let parts = if parts.is_empty() { String::new() } else { format!("; {}", parts.join("; ")) };
    format!(" ({} → {} findings{parts})", findings(&a), findings(&b))
}

/// The lines of the section with this heading.
fn body<'a>(list: &'a [(&'a str, Vec<&'a str>)], heading: &str) -> Option<&'a Vec<&'a str>> {
    list.iter().find(|(h, _)| *h == heading).map(|(_, lines)| lines)
}

/// The headings of `these` sections of a kind (`finding `, `plan `) that `those` doesn't have.
fn missing_from<'a>(these: &[(&'a str, Vec<&'a str>)], those: &[(&str, Vec<&str>)], kind: &str) -> Vec<&'a str> {
    these.iter().map(|(h, _)| *h).filter(|h| h.starts_with(kind) && !those.iter().any(|(t, _)| t == h)).collect()
}

/// Ids for a sentence: the first eight, and how many more.
fn ids(headings: &[&str]) -> String {
    let shown: Vec<&str> = headings.iter().take(8).map(|h| h.split_once(' ').map_or(*h, |(_, id)| id)).collect();
    let more = headings.len() - shown.len();
    if more > 0 { format!("{} and {more} more", shown.join(", ")) } else { shown.join(", ") }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Same(usize, usize),
    Del(usize),
    Ins(usize),
}

/// Lines of `a` and `b` matched up: the common start and end as they are, the middle by its longest common
/// subsequence (or, when that would take too much memory, all of `a`'s middle out and `b`'s in).
fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let start = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let end = a[start..].iter().rev().zip(b[start..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[start..a.len() - end], &b[start..b.len() - end]);
    let mut ops: Vec<Op> = (0..start).map(|i| Op::Same(i, i)).collect();
    let (n, m) = (am.len(), bm.len());
    if n * m <= 4_000_000 {
        // lcs[i * (m + 1) + j]: the longest common subsequence of am[i..] and bm[j..].
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * (m + 1) + j] = if am[i] == bm[j] {
                    lcs[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Same(start + i, start + j));
                (i, j) = (i + 1, j + 1);
            } else if j == m || (i < n && lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
                ops.push(Op::Del(start + i));
                i += 1;
            } else {
                ops.push(Op::Ins(start + j));
                j += 1;
            }
        }
    } else {
        ops.extend((start..start + n).map(Op::Del));
        ops.extend((start..start + m).map(Op::Ins));
    }
    ops.extend((0..end).map(|k| Op::Same(a.len() - end + k, b.len() - end + k)));
    ops
}

/// The first changes from `old` (the golden file) to `new` (the reading now), unified-style: each hunk headed by the
/// section it is in (`=== finding <id>`), `SHOWN` changed lines at most.
fn short_diff(old: &str, new: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let ops = script(&a, &b);
    let changed: Vec<usize> = (0..ops.len()).filter(|&k| !matches!(ops[k], Op::Same(..))).collect();
    let mut out = format!("--- accepted ({} lines)\n+++ now ({} lines)\n", a.len(), b.len());
    let mut shown = 0;
    let mut k = 0;
    while k < changed.len() && shown < SHOWN {
        // A hunk: this change, the ones within twice the context of it, and the context around them.
        let first = changed[k];
        let mut last = first;
        while k + 1 < changed.len() && changed[k + 1] <= last + 2 * CONTEXT + 1 {
            k += 1;
            last = changed[k];
        }
        k += 1;
        let (from, to) = (first.saturating_sub(CONTEXT), (last + CONTEXT + 1).min(ops.len()));
        // Where the hunk starts in each file: the lines of each the ops before it went through.
        let ai = ops[..from].iter().filter(|op| !matches!(op, Op::Ins(_))).count();
        let bj = ops[..from].iter().filter(|op| !matches!(op, Op::Del(_))).count();
        // The section the first change is in.
        let section = match ops[first] {
            Op::Del(i) => section_of(&a, i),
            Op::Ins(j) => section_of(&b, j),
            Op::Same(..) => None,
        };
        out.push_str(&format!("@@ -{} +{} @@", ai + 1, bj + 1));
        if let Some(section) = section {
            out.push_str(&format!(" {section}"));
        }
        out.push('\n');
        for &op in &ops[from..to] {
            let line = match op {
                Op::Same(i, _) => format!(" {}", a[i]),
                Op::Del(i) => format!("-{}", a[i]),
                Op::Ins(j) => format!("+{}", b[j]),
            };
            if !line.starts_with(' ') {
                if shown == SHOWN {
                    break;
                }
                shown += 1;
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    if changed.len() > shown {
        out.push_str(&format!("… and {} more changed lines\n", changed.len() - shown));
    }
    out
}

/// The `=== ` line that heads line `at`'s section.
fn section_of<'a>(lines: &[&'a str], at: usize) -> Option<&'a str> {
    lines[..=at].iter().rev().find(|l| l.starts_with("=== ")).copied()
}

#[test]
fn a_change_reads_as_a_short_diff() {
    let old = "=== finding a\ntitle: one\nsame\nsame\nsame\nsame\nsame\nsame\nsame\nsame\n=== finding b\ntitle: two\n";
    let new =
        "=== finding a\ntitle: uno\nsame\nsame\nsame\nsame\nsame\nsame\nsame\nsame\n=== finding b\ntitle: two\nmore\n";
    let diff = short_diff(old, new);
    assert!(diff.contains("@@ -1 +1 @@ === finding a\n === finding a\n-title: one\n+title: uno\n same\n"), "{diff}");
    assert!(diff.contains("@@ -10 +10 @@ === finding b\n same\n === finding b\n title: two\n+more\n"), "{diff}");
    assert!(!diff.contains("more changed lines"), "{diff}");
}
