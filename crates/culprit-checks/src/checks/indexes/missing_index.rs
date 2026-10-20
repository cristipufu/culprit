//! The `missing-index` check: Index suggestions recorded by SQL Server's optimizer, merged with each other and checked against existing indexes.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn missing_index_info() -> CheckInfo {
    CheckInfo {
        id: "missing-index".into(),
        title: "Missing indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::MissingIndexes],
        description: "Index suggestions recorded by SQL Server's optimizer, merged with each other and checked against existing indexes.".into(),
    }
}

check!(MissingIndexCheck: missing_index_info, missing_index, &[Evidence::IndexCatalog, Evidence::Unknown]);

pub(crate) struct Candidate {
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) equality: Vec<String>,
    pub(crate) inequality: Vec<String>,
    pub(crate) include: Vec<String>,
    pub(crate) seeks: f64,
    pub(crate) scans: f64,
    pub(crate) weight: f64,
    pub(crate) impact_pct: f64,
    pub(crate) query_hashes: Vec<String>,
    /// The optimizer requests merged into this candidate, its own first.
    pub(crate) requests: Vec<MissingIndex>,
}

impl Candidate {
    fn from(m: &MissingIndex) -> Candidate {
        Candidate {
            schema: m.schema.clone(),
            table: m.table.clone(),
            equality: m.equality.clone(),
            inequality: m.inequality.clone(),
            include: m.include.clone(),
            seeks: m.seeks,
            scans: m.scans,
            weight: m.avg_query_cost * (m.avg_impact_pct / 100.0) * (m.seeks + m.scans),
            impact_pct: m.avg_impact_pct,
            query_hashes: m.query_hashes.clone(),
            requests: vec![m.clone()],
        }
    }

    pub(crate) fn keys(&self) -> Vec<String> {
        self.equality.iter().chain(self.inequality.iter()).cloned().collect()
    }

    fn same_table(&self, other: &Candidate) -> bool {
        self.schema.eq_ignore_ascii_case(&other.schema) && self.table.eq_ignore_ascii_case(&other.table)
    }

    /// The index built for `other` (its [`Candidate::keys`], the order the DDL writes them) serves `self` as well as
    /// self's own would: it leads with self's equality columns, in any order, then self's range columns in order. Only
    /// then can self's queries seek it; an index that merely contains self's columns further along (`(CustomerId,
    /// Status)` for a request on `Status`) could only be scanned, so the two stay apart and keep their own counts.
    fn covered_by(&self, other: &Candidate) -> bool {
        if !self.same_table(other) {
            return false;
        }
        let theirs = other.keys();
        let n = self.equality.len();
        theirs.len() >= n + self.inequality.len()
            && lower_set(&theirs[..n]) == lower_set(&self.equality)
            && self.inequality.iter().zip(&theirs[n..]).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    fn absorb(&mut self, other: Candidate) {
        let keys = lower_set(&self.keys());
        for c in other.include.into_iter().chain(other.inequality) {
            let lc = c.to_ascii_lowercase();
            if !keys.contains(&lc) && !self.include.iter().any(|i| i.eq_ignore_ascii_case(&c)) {
                self.include.push(c);
            }
        }
        self.seeks += other.seeks;
        self.scans += other.scans;
        self.weight += other.weight;
        self.impact_pct = self.impact_pct.max(other.impact_pct);
        self.query_hashes.extend(other.query_hashes);
        self.requests.extend(other.requests);
    }
}

pub(crate) fn merge_candidates(snapshot: &Snapshot) -> Vec<Candidate> {
    let mut all: Vec<Candidate> = snapshot
        .missing_indexes
        .iter()
        .filter(|m| m.seeks + m.scans >= 10.0 && m.avg_impact_pct >= 10.0)
        .map(Candidate::from)
        .collect();
    all.sort_by(|a, b| b.keys().len().cmp(&a.keys().len()).then(b.weight.total_cmp(&a.weight)));
    let mut merged: Vec<Candidate> = Vec::new();
    for c in all {
        match merged.iter_mut().find(|m| c.covered_by(m)) {
            Some(target) => target.absorb(c),
            None => merged.push(c),
        }
    }
    merged
}

/// An existing index whose leading keys already match the suggestion: the fix is to widen it (or, disabled, to bring
/// it back), not add another. A filtered index only when every query that `asking` names fits its filter as written
/// (`is_deleted = 0` in the query): widening an index the queries can't use wouldn't help them, and with no query to
/// show, nothing says they can. An enabled index is preferred to a disabled one with the same keys.
pub(crate) fn existing_match<'a>(
    snapshot: &'a Snapshot,
    c: &Candidate,
    asking: &[&QueryStat],
) -> Option<&'a IndexStat> {
    same_keys_on_table(snapshot, c)
        .filter(|i| match i.filter.as_deref() {
            None => true,
            Some(filter) => fits_filter(&c.table, filter, asking),
        })
        .min_by_key(|i| i.is_disabled)
}

/// A filtered index that would serve the suggestion but that [`existing_match`] and [`overlapping_index`] passed over,
/// with what it has of it ("has these keys", "leads with tenant_id and account_id"): the queries don't ask for only
/// the rows it holds, or Culprit can't show that they do.
pub(super) fn filtered_bystander<'a>(
    snapshot: &'a Snapshot,
    c: &Candidate,
    asking: &[&QueryStat],
) -> Option<(&'a IndexStat, String)> {
    let passed_over = |i: &&IndexStat| i.filter.as_deref().is_some_and(|f| !fits_filter(&c.table, f, asking));
    if let Some(i) = same_keys_on_table(snapshot, c).find(passed_over) {
        return Some((i, "has these keys".into()));
    }
    snapshot.indexes.iter().filter(|i| on_table(i, c) && !i.is_disabled).filter(passed_over).find_map(|i| {
        let overlap = overlap(i, c).filter(Overlap::serves)?;
        Some((i, format!("leads with {}", format::list(&overlap.seeks))))
    })
}

fn on_table(i: &IndexStat, c: &Candidate) -> bool {
    i.schema.eq_ignore_ascii_case(&c.schema) && i.table.eq_ignore_ascii_case(&c.table)
}

/// What an existing index does for a suggestion: the columns it seeks on (its leading keys that the suggestion's
/// equality columns are, in any order, then its first range column if that comes next), those its filter fixes
/// (`is_deleted = 0`), and those it does neither for, which it checks on the rows it finds: equality columns, and a
/// range column it has no key for after them.
struct Overlap {
    seeks: Vec<String>,
    filtered: Vec<String>,
    missing: Vec<String>,
    /// The request's range column is among `missing`: the index has no key for it after the equality columns.
    range_missing: bool,
}

impl Overlap {
    /// It finds the suggestion's rows by all of its columns but at most one, and by at least two: what's left is
    /// checked on the rows it finds, which costs little once it narrows by the rest. A seek on one of several columns
    /// may read many rows the others reject; that is what the optimizer asked about.
    fn serves(&self) -> bool {
        self.seeks.len() + self.filtered.len() >= 2 && self.missing.len() <= 1
    }
}

fn overlap(i: &IndexStat, c: &Candidate) -> Option<Overlap> {
    // More than one range column can't all be sought, or checked as cheaply as one: the request is for its own index.
    if c.equality.is_empty() || c.inequality.len() > 1 {
        return None;
    }
    let asked = |column: &str| c.equality.iter().any(|e| e.eq_ignore_ascii_case(column));
    let keys = i.key_names();
    let mut seeks: Vec<String> = keys.iter().take_while(|k| asked(k)).map(|k| k.to_string()).collect();
    if seeks.is_empty() {
        return None;
    }
    let filtered: Vec<String> = i
        .filter
        .as_deref()
        .and_then(filter_equalities)
        .unwrap_or_default()
        .into_iter()
        .map(|(column, _)| column)
        .filter(|column| asked(column) && !seeks.iter().any(|s| s.eq_ignore_ascii_case(column)))
        .collect();
    let mut missing: Vec<String> = c
        .equality
        .iter()
        .filter(|e| !seeks.iter().chain(&filtered).any(|s| s.eq_ignore_ascii_case(e)))
        .cloned()
        .collect();
    // The range: sought when it is the key right after every equality column, otherwise checked on the rows found.
    let mut range_missing = false;
    if let Some(range) = c.inequality.first() {
        let next = keys.get(seeks.len()).filter(|k| missing.is_empty() && k.eq_ignore_ascii_case(range));
        match next {
            Some(_) => seeks.push(range.clone()),
            None => {
                missing.push(range.clone());
                range_missing = true;
            }
        }
    }
    Some(Overlap { seeks, filtered, missing, range_missing })
}

/// An existing index that finds a request's rows by every one of its equality columns (its keys and its filter) but
/// has no key for its range, when the request is for an index of its own: the new index seeks the range as well. One a
/// query can use (unfiltered, or its filter as the queries write it); the one that seeks on the most.
fn lacks_only_the_range<'a>(
    snapshot: &'a Snapshot,
    c: &Candidate,
    asking: &[&QueryStat],
) -> Option<(&'a IndexStat, Overlap)> {
    snapshot
        .indexes
        .iter()
        .filter(|i| on_table(i, c) && !i.is_disabled)
        .filter(|i| i.filter.as_deref().is_none_or(|f| fits_filter(&c.table, f, asking)))
        .filter_map(|i| {
            let o = overlap(i, c).filter(|o| o.range_missing && o.missing.len() == 1 && o.serves())?;
            Some((i, o))
        })
        .max_by_key(|(i, o)| (o.seeks.len() + o.filtered.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len())))
}

/// The enabled index on a request's table that already finds its rows by all of its columns but one
/// ([`Overlap::serves`]), whether or not its queries are shown to fit its filter: the one that seeks on the most,
/// unfiltered first, then the narrowest. Said where there is nothing to create, as what the lookups have meanwhile.
fn serving_index<'a>(snapshot: &'a Snapshot, c: &Candidate) -> Option<(&'a IndexStat, Overlap)> {
    snapshot
        .indexes
        .iter()
        .filter(|i| on_table(i, c) && !i.is_disabled)
        .filter_map(|i| Some((i, overlap(i, c).filter(Overlap::serves)?)))
        .max_by_key(|(i, o)| (o.seeks.len() + o.filtered.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len())))
}

/// An existing index that already finds a suggestion's rows by all of its equality columns but one ([`Overlap::serves`]),
/// when [`existing_match`] found none with its keys: including the column it lacks, and the suggestion's included
/// columns, serves the queries with one index fewer to maintain than a new one. Only one Culprit can widen without
/// changing what it is (not the clustered index, a primary key or a unique one), and a filtered one only for queries
/// that ask for the rows it holds ([`fits_filter`]). The one that seeks on the most, unfiltered first, then the
/// narrowest; with the columns to include.
fn overlapping_index<'a>(
    snapshot: &'a Snapshot,
    c: &Candidate,
    asking: &[&QueryStat],
) -> Option<(&'a IndexStat, Overlap, Vec<String>)> {
    snapshot
        .indexes
        .iter()
        .filter(|i| on_table(i, c) && !i.is_disabled && !i.is_clustered && !i.is_primary_key && !i.is_unique)
        .filter(|i| i.filter.as_deref().is_none_or(|f| fits_filter(&c.table, f, asking)))
        .filter_map(|i| {
            // A range the index has no key for is the request's own index: checking it on every row the equality
            // columns find can cost what the range was asked for to spare.
            let overlap = overlap(i, c).filter(|o| o.serves() && !o.range_missing)?;
            let include: Vec<String> = overlap.missing.iter().chain(&c.include).cloned().collect();
            let add = lacking_includes(i, &include);
            (!add.is_empty()).then_some((i, overlap, add))
        })
        .max_by_key(|(i, o, _)| {
            (o.seeks.len() + o.filtered.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len()))
        })
}

/// The indexes on the suggestion's table whose leading keys are its equality columns (in any order), then its range.
pub(super) fn same_keys_on_table<'a>(snapshot: &'a Snapshot, c: &Candidate) -> impl Iterator<Item = &'a IndexStat> {
    let keys = c.keys();
    let (schema, table, equality) = (c.schema.clone(), c.table.clone(), c.equality.clone());
    snapshot.indexes.iter().filter(move |i| {
        let existing = i.key_names();
        i.schema.eq_ignore_ascii_case(&schema)
            && i.table.eq_ignore_ascii_case(&table)
            && existing.len() >= keys.len()
            && lower_set(&existing[..equality.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                == lower_set(&equality)
            && keys[equality.len()..].iter().zip(&existing[equality.len()..]).all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Every query in `asking` (at least one) compares each of the filter's columns of `table` to the filter's own literal.
pub(crate) fn fits_filter(table: &str, filter: &str, asking: &[&QueryStat]) -> bool {
    let Some(conditions) = filter_equalities(filter) else { return false };
    !asking.is_empty()
        && asking.iter().all(|q| {
            conditions
                .iter()
                .all(|(column, literal)| compared_to(&q.text, table, column, |value| same_literal(value, literal)))
        })
}

/// A filter as people read it: "is_deleted = 0", or the definition without its brackets.
pub(crate) fn readable_filter(filter: &str) -> String {
    match filter_equalities(filter) {
        Some(conditions) => {
            format::list(&conditions.iter().map(|(column, value)| format!("{column} = {value}")).collect::<Vec<_>>())
        }
        None => {
            let plain: String = filter.chars().filter(|c| !matches!(c, '[' | ']')).collect();
            plain.strip_prefix('(').and_then(|p| p.strip_suffix(')')).unwrap_or(&plain).to_string()
        }
    }
}

pub fn missing_index(snapshot: &Snapshot) -> Vec<Finding> {
    let queries = snapshot.queries();
    merge_candidates(snapshot).into_iter().map(|c| missing_index_finding(snapshot, queries, c)).collect()
}

pub(super) fn missing_index_finding(snapshot: &Snapshot, queries: &[QueryStat], mut c: Candidate) -> Finding {
    let keys = c.keys();
    let table_name = format!("{}.{}", c.schema, c.table);
    let key_text = format::list(&keys);
    let mut f = finding(
        "missing-index",
        &format!("{table_name}({})", keys.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    f.facts.index_keys = keys.clone();
    let linked = asking_queries(queries, &c);
    if let Some(miss) = parameterized_filter(snapshot, &c, &linked) {
        return parameterized_filter_finding(snapshot, f, &c, &linked, &miss);
    }
    let table = snapshot.table(&c.schema, &c.table);
    let rows = table.and_then(|t| t.rows);
    let lookups = c.seeks + c.scans;
    // Asked for only before the window: history once that is a day back, otherwise said and kept active.
    let asked = wanted_before_window(snapshot, &c);
    let lately = asked.as_ref().filter(|a| !a.stopped());

    // Asked for by statements that all stopped running: told in the past, as refine then marks it.
    let history = crate::all_stopped(snapshot, &linked) || asked.as_ref().is_some_and(LastAsked::stopped);
    let is = if history { "was" } else { "is" };
    f.title = format!("{} {is} searched by {} without a supporting index", c.table, key_text);
    let mut summary = format!(
        "SQL Server's optimizer asked for an index on {} ({}) {} times {}. Without it, each of those lookups {} more of {}{} than it {} to. It {} the index would make them about {:.0}% cheaper.",
        table_name,
        key_text,
        format::count(lookups),
        since_phrase(snapshot),
        if history { "read" } else { "reads" },
        table_name,
        rows.map(|r| format!(" ({} rows)", format::count(r))).unwrap_or_default(),
        if history { "needed" } else { "needs" },
        if history { "estimated" } else { "estimates" },
        c.impact_pct
    );
    match query_impact(snapshot, &linked) {
        Some(impact) => {
            summary.push_str(&format!(
                " The {} that asked for it {} for {} of database time.",
                if linked.len() == 1 { "query".to_string() } else { format!("{} queries", linked.len()) },
                if history {
                    "accounted"
                } else if linked.len() == 1 {
                    "accounts"
                } else {
                    "account"
                },
                format::percent(impact.share)
            ));
            apply_query_impact(&mut f, &impact);
            // What the index saves is the queries' time times the optimizer's estimate of the gain: 11% of a query at
            // 0.01% of database time is nothing to rank or raise above Low.
            let gain = (c.impact_pct / 100.0).clamp(0.0, 1.0);
            f.impact.score = score_for_share(impact.share * gain);
            f.severity = request_severity(snapshot, &c, &linked);
            // How sure, not how much (the severity says that): the optimizer's estimate is a share of what its
            // queries read. Where their runs measured real reading ([`MEASURED_READS`] pages a run) of a table of
            // SCAN_ROWS rows or more, a large gain is certain; on a small table, or runs that read a few pages, a
            // large share of very little is a guess.
            let measured = reads_per_run(&linked).is_some_and(|pages| pages >= MEASURED_READS);
            f.confidence = if c.impact_pct >= 50.0 && measured && rows.is_some_and(|r| r >= SCAN_ROWS) {
                Confidence::High
            } else {
                Confidence::Medium
            };
        }
        None if linked.is_empty() && snapshot.workload.is_some() => {
            summary.push_str(&match lately {
                Some(a) => format!(" {}", a.lately_sentence()),
                None => " None of the queries in this scan asked for it: the requests came from plans that ran before the scan's window, or that Query Store no longer keeps.".to_string(),
            });
            f.severity = request_severity(snapshot, &c, &linked);
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
        None => {
            f.severity = request_severity(snapshot, &c, &linked);
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
    }
    // Azure SQL weighs the same index over a longer history than the optimizer's counters, which reset on restart.
    if let Some(r) = crate::tuning::recommended_create(snapshot, &c.schema, &c.table, &keys) {
        summary.push_str(&format!(
            " Azure SQL's automatic tuning recommends the same index{}.",
            r.score.map(|s| format!(", scoring it {s:.0} out of 100")).unwrap_or_default()
        ));
        f.confidence = Confidence::High;
    }
    f.summary = summary;
    f.queries = query_refs(&linked);
    f.metrics.push(Metric::new("Lookups that wanted it", lookups, Unit::Count));
    f.metrics.push(Metric::new("Estimated improvement", c.impact_pct / 100.0, Unit::Ratio));
    if let Some(r) = rows {
        f.metrics.push(Metric::new("Rows in table", r, Unit::Rows));
    }
    if let Some(share) = f.impact.db_time_share {
        f.metrics.insert(0, Metric::new("Share of DB time", share, Unit::Ratio));
    }

    match &asked {
        Some(a) if a.stopped() => {
            // An old request is history even if an associated query hash still runs with another plan.
            f.actionability = culprit_core::Actionability::History;
            f.inactive = crate::all_stopped(snapshot, &linked);
            f.title = format!("{} was searched by {} without a supporting index", c.table, key_text);
            f.severity = Severity::Low;
            f.impact.score *= 0.25;
            f.last_seen = Some(a.last.clone());
            f.caveats.insert(
                0,
                format!(
                    "SQL Server's optimizer last asked for this index on {}, {} before this scan's data ends: the \
                     queries that wanted it have most likely changed or stopped running. Its requests are kept until \
                     the server restarts.",
                    format::friendly_date(&a.last, &snapshot.taken_at),
                    format::duration(a.silence * 1000.0),
                ),
            );
        }
        // Said in the summary when no query of the window is linked to it; beside linked ones, as a caveat.
        Some(a) if !linked.is_empty() => f.caveats.insert(0, a.lately_sentence()),
        _ => {}
    }

    if too_small_to_index(snapshot, &c, &linked) {
        return too_small_finding(snapshot, f, &c, &linked);
    }
    if let Some(a) = asked.as_ref().filter(|_| !f.inactive) {
        f.actionability =
            if a.stopped() { culprit_core::Actionability::History } else { culprit_core::Actionability::Information };
        f.severity = Severity::Low;
        f.title = format!("SQL Server previously requested an index on {} ({key_text})", c.table);
        f.summary = format!(
            "SQL Server last requested this index {}, before {}. The {} requests are cumulative {}. Associated statements may still run, but their activity does not establish that this historical request is still needed in the selected period.",
            crate::queries::when_phrase(&a.last),
            a.window,
            format::count(lookups),
            since_phrase(snapshot)
        );
        f.impact.db_time_share = None;
        f.impact.time_ms_per_day = None;
        f.impact.score = base_score(Severity::Low);
        f.impact.label = "Historical optimizer request".into();
        f.metrics.retain(|m| m.label != "Share of DB time");
        for r in &mut f.queries {
            r.note = Some("Associated with a request made before the selected window; current query activity does not date that request.".into());
        }
        f.fix = Some(Fix { summary: "Keep this candidate as context. Check representative plans and reads in the period that matters before choosing an index. If the optimizer requests it again, compare the measured benefit with index storage and write maintenance.".into(), sql: None, cost: None });
        f.verify = vec!["A later scan must show a new request inside its workload window and enough measured work before this becomes an index recommendation.".into()];
        return f;
    }
    // A request no query in the window made is history: it gets no index of its own.
    if f.inactive {
        let writes = measured_writes(snapshot, &c.schema, &c.table)
            .filter(|(writes, _)| *writes > 0.0)
            .map(|(writes, days)| {
                format!(
                    ", and an index on {key_text} would add to every write on {table_name} ({})",
                    how_many(snapshot, writes, days, "write", "writes")
                )
            })
            .unwrap_or_default();
        f.fix = Some(Fix {
            summary: format!(
                "Nothing to create unless the statements that wanted it come back: they have most likely changed or \
                 stopped running{writes}. If they come back, the optimizer asks for it again, and a scan then shows \
                 them and the index to create."
            ),
            sql: None,
            cost: None,
        });
        f.verify = vec![
            "Nothing to check unless they come back: if the optimizer asks for this index again within a scan's \
             window, this finding names the queries behind it and gives the index to create."
                .into(),
        ];
        return f;
    }
    // Columns to include matter only for an index worth creating.
    if let Some(note) = drop_large_includes(snapshot, &c.schema, &c.table, &mut c.include) {
        f.caveats.push(note);
    }
    let wide_include = c.include.len() > MAX_INCLUDE;
    if wide_include {
        f.caveats.push(format!(
            "SQL Server also suggested including {} more columns. That makes a large, write-heavy index; the fix keeps the keys only. Add INCLUDE columns only for the columns the hot queries actually select.",
            c.include.len()
        ));
        c.include.clear();
    }

    let fixed = candidate_fix(snapshot, &c, &linked);
    // A widen rebuilds an existing index under its own name: the queries seek that one, not a new one.
    let first = match (&fixed.widens, fixed.fix.sql.is_some()) {
        (Some(name), true) => {
            format!("After testing the rebuild of {name}, capture a newly compiled plan under representative load. A stored suggestion can remain in old plan XML.")
        }
        (Some(name), false) => format!("Check whether newly compiled plans use {name}; an old stored suggestion alone does not establish a current gap."),
        (None, _) => "Test the candidate index with representative parameters and load, then capture a newly compiled plan. Old stored suggestions need not disappear.".into(),
    };
    f.verify = vec![first];
    if !linked.is_empty() {
        f.verify.push(format!(
            "Check whether the queries listed here use {} and compare reads and execution time under equivalent parameters and load, including index storage and write cost.",
            fixed.widens.as_deref().unwrap_or("the new index")
        ));
    }
    f.fix = Some(fixed.fix);
    f.caveats.push("Missing-index suggestions are per-query and ignore the cost of maintaining the index; review overlapping suggestions together.".into());
    f
}

/// When SQL Server's optimizer last wanted an index that none of the window's queries wanted.
pub(super) struct LastAsked {
    /// The latest of the merged requests' last use.
    pub(super) last: String,
    /// Seconds from then to the end of the scan's data.
    pub(super) silence: f64,
    /// The window as words ("the last 2 hours").
    pub(super) window: String,
}

impl LastAsked {
    /// History: the queries that wanted it stopped, said only once the silence covers a day
    /// ([`crate::refine::silent_long_enough`], the rule for statements too).
    pub(super) fn stopped(&self) -> bool {
        crate::refine::silent_long_enough(self.silence)
    }

    /// Under a day: none of the window's statements asked for it, too soon to say they stopped.
    pub(super) fn lately_sentence(&self) -> String {
        let possessive =
            if self.window.ends_with('s') { format!("{}'", self.window) } else { format!("{}'s", self.window) };
        format!(
            "None of {possessive} statements asked for it: SQL Server's optimizer last did {}, {} before this scan's \
             data ends, too short a time to say the statements that wanted it stopped.",
            crate::queries::when_phrase(&self.last),
            format::duration(self.silence * 1000.0),
        )
    }
}

/// When SQL Server's optimizer last wanted the index, when that is before the scan's window (or, without query
/// history, before the last week: a weekly job has had its chance): none of the window's queries wanted it. `None`
/// when one of the merged requests was wanted within it, or when it isn't known when.
pub(super) fn wanted_before_window(snapshot: &Snapshot, c: &Candidate) -> Option<LastAsked> {
    let (end, start, window) = match &snapshot.workload {
        Some(w) => {
            let end = parse_epoch_seconds(w.window_end.as_deref().unwrap_or(&snapshot.taken_at))?;
            (end, end - w.measurement_hours() * 3600.0, w.window_phrase())
        }
        None => {
            let end = parse_epoch_seconds(&snapshot.taken_at)?;
            (end, end - MIN_INDEX_DAYS * 86_400.0, "the last week".to_string())
        }
    };
    let mut latest: Option<(f64, &str)> = None;
    // Even a request below the candidate's count threshold proves that this same key was wanted more recently.
    let same_key = snapshot.missing_indexes.iter().filter(|m| {
        m.schema.eq_ignore_ascii_case(&c.schema)
            && m.table.eq_ignore_ascii_case(&c.table)
            && lower_set(&m.equality) == lower_set(&c.equality)
            && m.inequality.len() == c.inequality.len()
            && m.inequality.iter().zip(&c.inequality).all(|(a, b)| a.eq_ignore_ascii_case(b))
    });
    for m in c.requests.iter().chain(same_key) {
        let text = m.last_used.as_deref()?;
        let at = parse_epoch_seconds(text)?;
        if latest.is_none_or(|(l, _)| at > l) {
            latest = Some((at, text));
        }
    }
    let (at, text) = latest?;
    (at < start).then(|| LastAsked { last: text.to_string(), silence: end - at, window })
}

/// What a request's fix is, given the table's indexes, and the index it changes instead of adding one.
pub(crate) struct CandidateFix {
    pub(crate) fix: Fix,
    /// The existing index the fix widens, brings back, or that already starts with the keys.
    pub(crate) widens: Option<String>,
}

/// The fix for an optimizer request (its include list already trimmed): widen the index that has its keys, bring a
/// disabled one back, point at one that already leads with them, or create a new one. The missing-index finding and
/// the index advisor both take it from here, so they never disagree.
pub(crate) fn candidate_fix(snapshot: &Snapshot, c: &Candidate, linked: &[&QueryStat]) -> CandidateFix {
    let keys = c.keys();
    let create = |why: Option<String>| {
        new_index_fix(snapshot, &c.schema, &c.table, &keys, &c.include, linked, format!(
            "{}Create an index on {}{}.{}{}",
            why.map(|w| format!("{w} ")).unwrap_or_default(),
            format::list(&keys),
            if c.include.is_empty() { String::new() } else { format!(", including {}", format::list(&c.include)) },
            // Key order is a choice only among two or more columns compared with a value.
            if c.equality.len() >= 2 {
                " Equality columns come first; put the most selective one at the front if you know it."
            } else {
                ""
            },
            filtered_bystander(snapshot, c, linked)
                .map(|(i, has)| {
                    let rows = format!(
                        " {} {has} but holds only the rows where {}",
                        i.name,
                        readable_filter(i.filter.as_deref().unwrap_or_default())
                    );
                    match linked.len() {
                        0 => format!("{rows}, and Culprit can't show that these lookups ask for only those."),
                        1 => format!("{rows}, which the query below doesn't ask for."),
                        _ => format!("{rows}, which the queries below don't ask for."),
                    }
                })
                .or_else(|| {
                    let (i, o) = lacks_only_the_range(snapshot, c, linked)?;
                    Some(format!(
                        " {} already finds these rows by {}{}, but has no key for {}: this index seeks the range as well.",
                        i.name,
                        format::list(&o.seeks),
                        i.filter
                            .as_deref()
                            .map(|f| format!(", in the rows where {}", readable_filter(f)))
                            .unwrap_or_default(),
                        format::list(&o.missing)
                    ))
                })
                .unwrap_or_default()
        ))
    };
    let Some(existing) = existing_match(snapshot, c, linked) else {
        if let Some((index, overlap, add)) = overlapping_index(snapshot, c, linked)
            && let Widening::Sql(fix) = widen_index(snapshot, index, &add)
        {
            let filter = index
                .filter
                .as_deref()
                .map(|f| format!(", in the rows where {}", readable_filter(f)))
                .unwrap_or_default();
            let lead = format!("{} already finds these rows by {}{filter}", index.name, format::list(&overlap.seeks));
            let summary = if overlap.missing.is_empty() {
                format!(
                    "{lead}. Rebuild it with {} included rather than add an index, so it covers {} without going back to the table.",
                    format::list(&add),
                    if linked.len() == 1 { "the query below" } else { "the queries below" }
                )
            } else {
                format!(
                    "{lead}: all the request asks for but {}. Widen it rather than add an index: with {} included, it checks {} on the rows it finds without going back to the table, and there is one index fewer to maintain.",
                    format::list(&overlap.missing),
                    format::list(&add),
                    format::list(&overlap.missing)
                )
            };
            return CandidateFix {
                fix: Fix {
                    summary: format!(
                        "{summary} The statement keeps its keys, their order, its filter and the columns it already includes; restate DATA_COMPRESSION, FILLFACTOR or its partition scheme in it if the index uses them."
                    ),
                    ..fix
                },
                widens: Some(index.name.clone()),
            };
        }
        return CandidateFix { fix: create(None), widens: None };
    };
    let widened = |fix: Fix| CandidateFix { fix, widens: Some(existing.name.clone()) };
    // The clustered index holds every column: with its keys in front, the request isn't for columns to include.
    if existing.is_disabled || (!lacking_includes(existing, &c.include).is_empty() && !existing.is_clustered) {
        return match widen_index(snapshot, existing, &c.include) {
            Widening::Sql(fix) => widened(fix),
            Widening::Refused(why) => CandidateFix { fix: create(Some(why)), widens: None },
        };
    }
    widened(Fix {
        summary: format!(
            "{} already starts with these columns, so the remaining gap is small. Check whether the queries below use it; if not, look at their predicates for type mismatches or functions on the column.",
            existing.name
        ),
        sql: None,
        cost: None,
    })
}

/// Tables of this many rows or more: reading one in full where a seek would do costs something (`plan::SCAN_ROWS`, the
/// bar a plan's note on a whole-table read uses too).
pub(crate) use culprit_core::plan::SCAN_ROWS;

/// Pages a run from which what the queries behind a request read is real reading, not the few pages any lookup reads.
const MEASURED_READS: f64 = 100.0;

/// The pages a run the queries behind a request read, over all their runs; `None` where the engine doesn't count them.
fn reads_per_run(linked: &[&QueryStat]) -> Option<f64> {
    let runs: f64 = linked.iter().map(|q| q.executions).sum();
    let reads: f64 = linked.iter().map(|q| q.logical_reads).sum::<Option<f64>>()?;
    (runs > 0.0).then(|| reads / runs)
}

/// How much an optimizer request is worth: what the queries that asked for it would save (their time times its
/// estimated gain), or, with none of them in the window, how often it was asked for.
fn request_severity(snapshot: &Snapshot, c: &Candidate, linked: &[&QueryStat]) -> Severity {
    match query_impact(snapshot, linked) {
        // What the index would save of their share, graded on the basis that share is on (Culprit's scans where they
        // measured them): never the scans' share beside the time their totals hold.
        Some(impact) => {
            let gain = (c.impact_pct / 100.0).clamp(0.0, 1.0);
            severity_for_statement_share(snapshot, linked, impact.share * gain)
        }
        None => unlinked_severity(snapshot, c, c.seeks + c.scans),
    }
}

/// Whether a request is on a table too small for an index to pay: under [`SCAN_ROWS`] rows reading it whole costs
/// little, so unless what its queries would save comes to real time (above Low), or Azure SQL, weighing a longer
/// history, recommends the same index, an index costs every write more than it saves. The missing-index finding and the
/// index advisor both ask this, so neither advises what the other calls not worth it.
pub(crate) fn too_small_to_index(snapshot: &Snapshot, c: &Candidate, linked: &[&QueryStat]) -> bool {
    snapshot.table(&c.schema, &c.table).and_then(|t| t.rows).is_some_and(|rows| rows < SCAN_ROWS)
        && request_severity(snapshot, c, linked) == Severity::Low
        && crate::tuning::recommended_create(snapshot, &c.schema, &c.table, &c.keys()).is_none()
}

/// The finding for a request on a table too small for an index to pay ([`too_small_to_index`]): what the optimizer
/// asked for, and why there is nothing to create.
fn too_small_finding(snapshot: &Snapshot, mut f: Finding, c: &Candidate, linked: &[&QueryStat]) -> Finding {
    f.actionability = culprit_core::Actionability::Information;
    let rows = snapshot.table(&c.schema, &c.table).and_then(|t| t.rows).unwrap_or(0.0);
    let table_name = format!("{}.{}", c.schema, c.table);
    // Requests that stopped (see `wanted_before_window`) are told in the past; the table's size is still now.
    f.title = format!(
        "SQL Server {} for an index on {}, which is too small for one to pay off",
        if f.inactive || crate::all_stopped(snapshot, linked) { "asked" } else { "asks" },
        c.table
    );
    f.severity = Severity::Low;
    f.confidence = Confidence::Medium;
    f.impact.score *= 0.25;
    let time_ms: f64 = linked.iter().map(|q| q.total_time_ms).sum();
    let took = match (linked.len(), snapshot.workload.as_ref()) {
        (0, _) | (_, None) => String::new(),
        (n, Some(w)) => format!(
            ", and the {} that asked for it took {} in {}",
            if n == 1 { "query".to_string() } else { format!("{n} queries") },
            format::duration(time_ms),
            w.window_phrase()
        ),
    };
    let writes = measured_writes(snapshot, &c.schema, &c.table)
        .filter(|(writes, _)| *writes > 0.0)
        .map(|(writes, days)| format!(" ({})", how_many(snapshot, writes, days, "write", "writes")))
        .unwrap_or_default();
    // An index the table has that already narrows these lookups by most of their columns: what they can use meanwhile.
    let existing = serving_index(snapshot, c)
        .map(|(i, o)| {
            format!(
                " {} already leads with {}{}.",
                i.name,
                format::list(&o.seeks),
                i.filter
                    .as_deref()
                    .map(|filter| format!(", for the rows where {}", readable_filter(filter)))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default();
    f.fix = Some(Fix {
        summary: format!(
            "Nothing to create while {table_name} stays this small: it holds {} rows{took}, so reading it whole costs \
             little, and an index on {} would add to every one of its writes{writes}.{existing} {}",
            format::count(rows),
            format::list(&c.keys()),
            if f.inactive {
                format!(
                    "Reconsider it only if the statements return and the optimizer asks again, and the table grows past {} rows or those lookups come to take real time.",
                    format::count(SCAN_ROWS)
                )
            } else {
                format!(
                    "Create one only if the table grows past {} rows or these lookups come to take real time.",
                    format::count(SCAN_ROWS)
                )
            }
        ),
        sql: None,
        cost: None,
    });
    f.verify = vec![format!(
        "Nothing to check while it stays this small: once {table_name} holds {} rows or more, a scan says whether the index is worth creating.",
        format::count(SCAN_ROWS)
    )];
    f.caveats.push("Missing-index suggestions are per-query and ignore the cost of maintaining the index; review overlapping suggestions together.".into());
    f
}

/// A request no query in the window shows: Medium only for a large estimated gain asked for often, a thousand
/// lookups a day over the hour or more its counters ran (they count from the restart, which may be months ago).
fn unlinked_severity(snapshot: &Snapshot, c: &Candidate, lookups: f64) -> Severity {
    let often = counter_rate_per_day(snapshot, lookups).is_some_and(|d| d >= 1000.0);
    if c.impact_pct >= 70.0 && often { Severity::Medium } else { Severity::Low }
}

/// The queries behind a candidate: linked by query_hash (SQL Server 2019+), or whose plan carries the same request.
pub(crate) fn asking_queries<'a>(queries: &'a [QueryStat], c: &Candidate) -> Vec<&'a QueryStat> {
    queries
        .iter()
        .filter(|q| q.query_hash.as_ref().is_some_and(|h| c.query_hashes.iter().any(|x| x.eq_ignore_ascii_case(h))))
        .chain(queries.iter().filter(|q| plan_asks_for(q, c)))
        .fold(Vec::new(), |mut acc, q| {
            if !acc.iter().any(|a: &&QueryStat| a.id == q.id) {
                acc.push(q);
            }
            acc
        })
}

pub(super) fn plan_asks_for(q: &QueryStat, c: &Candidate) -> bool {
    q.plan.as_ref().is_some_and(|p| {
        p.index_hints.iter().any(|h| {
            let table_ok = h.table.to_ascii_lowercase().replace(['[', ']'], "").ends_with(&format!(
                "{}.{}",
                c.schema.to_ascii_lowercase(),
                c.table.to_ascii_lowercase()
            ));
            // The same request: its equality columns and its range columns. Two requests on one table with no
            // equality (a range on created_at, another on shipped_at) are different indexes and different queries.
            let same = |equality: &[String], inequality: &[String]| {
                lower_set(&h.equality) == lower_set(equality) && lower_set(&h.inequality) == lower_set(inequality)
            };
            table_ok
                && (same(&c.equality, &c.inequality) || c.requests.iter().any(|r| same(&r.equality, &r.inequality)))
        })
    })
}

/// An existing filtered index that would serve a request if its queries wrote the filter as the index does. SQL Server
/// matches a filtered index to a query only through the literal in its filter (`is_deleted = 0`), never through a
/// parameter that could hold any value, so a query that passes the filter's column as a parameter makes the optimizer
/// ask for an index the table already has.
pub(crate) struct ParameterizedFilter<'a> {
    pub(crate) index: &'a IndexStat,
    /// The filter's conditions, each a column the queries pass as a parameter and the literal the index uses.
    pub(crate) conditions: Vec<(String, String)>,
    /// The request without the filter's columns: its equality columns in the index's order, then its range columns.
    pub(crate) keys: Vec<String>,
    /// Range columns the index has no key for after the equality ones: with the literal, it finds the rows by those
    /// and checks these on each.
    pub(crate) missing_range: Vec<String>,
}

/// What a request still needs once its queries write the filter as a literal.
pub(crate) enum Remainder {
    /// The index serves it all.
    Nothing,
    /// The index with these columns included: it has the keys, and reads them from the table for each row it finds.
    Include(Vec<String>),
    /// An index on these keys and included columns, filtered like the existing one: it has no key for the range.
    Index { keys: Vec<String>, include: Vec<String> },
}

impl ParameterizedFilter<'_> {
    pub(crate) fn columns(&self) -> Vec<String> {
        self.conditions.iter().map(|(column, _)| column.clone()).collect()
    }

    /// The filter as people write it: "is_deleted = 0".
    pub(crate) fn literals(&self) -> String {
        format::list(&self.conditions.iter().map(|(column, value)| format!("{column} = {value}")).collect::<Vec<_>>())
    }

    /// What's left of the request once the queries use the literal, given the columns it should include (`include`,
    /// already trimmed of large and surplus ones).
    pub(crate) fn remainder(&self, include: &[String]) -> Remainder {
        let filtered = |c: &String| self.conditions.iter().any(|(column, _)| column.eq_ignore_ascii_case(c));
        if !self.missing_range.is_empty() {
            let include = include
                .iter()
                .filter(|c| !filtered(c) && !self.keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
                .cloned()
                .collect();
            return Remainder::Index { keys: self.keys.clone(), include };
        }
        let has = |c: &String| {
            self.index.key_names().iter().any(|k| k.eq_ignore_ascii_case(c))
                || self.index.include.iter().any(|i| i.eq_ignore_ascii_case(c))
        };
        let missing: Vec<String> = include.iter().filter(|c| !filtered(c) && !has(c)).cloned().collect();
        if missing.is_empty() { Remainder::Nothing } else { Remainder::Include(missing) }
    }

    /// The DDL for [`Self::remainder`], and the index it rebuilds rather than adds; `None` when nothing is left. The
    /// filter is kept either way: an index keyed on the filter's column would hold every row the filter leaves out.
    pub(crate) fn remainder_sql(&self, snapshot: &Snapshot, remainder: &Remainder) -> Option<(String, Option<String>)> {
        let index = self.index;
        match remainder {
            Remainder::Nothing => None,
            Remainder::Include(columns) => {
                widen_fix(snapshot, index, columns).sql.map(|sql| (sql, Some(index.name.clone())))
            }
            Remainder::Index { keys, include } => Some((
                create_filtered_index_sql(
                    snapshot,
                    &index.schema,
                    &index.table,
                    keys,
                    include,
                    index.filter.as_deref()?,
                ),
                None,
            )),
        }
    }
}

/// The filtered index on the request's table whose filter's columns the asking queries compare to parameters, and whose
/// leading keys are the request's other equality columns. Its keys may lack the request's range columns, and it may
/// lack included columns: [`ParameterizedFilter::remainder`] says what's left.
pub(crate) fn parameterized_filter<'a>(
    snapshot: &'a Snapshot,
    c: &Candidate,
    asking: &[&QueryStat],
) -> Option<ParameterizedFilter<'a>> {
    // A disabled index serves nothing, however its queries write the filter.
    let on_table = |i: &&IndexStat| {
        i.schema.eq_ignore_ascii_case(&c.schema) && i.table.eq_ignore_ascii_case(&c.table) && !i.is_disabled
    };
    snapshot.indexes.iter().filter(on_table).find_map(|i| {
        let conditions = filter_equalities(i.filter.as_deref()?)?;
        // Every condition of the filter: the request asks for its column, and a query passes it as a parameter.
        let all_passed = conditions.iter().all(|(column, _)| {
            c.equality.iter().any(|k| k.eq_ignore_ascii_case(column))
                && asking.iter().any(|q| compares_to_parameter(&q.text, &c.table, column))
        });
        if !all_passed {
            return None;
        }
        let in_filter = |k: &&String| conditions.iter().any(|(column, _)| column.eq_ignore_ascii_case(k));
        let rest: Vec<String> = c.equality.iter().filter(|k| !in_filter(k)).cloned().collect();
        let range: Vec<String> = c.inequality.iter().filter(|k| !in_filter(k)).cloned().collect();
        let keys: Vec<String> = i.key_names().iter().map(|k| k.to_string()).collect();
        if rest.is_empty() || keys.len() < rest.len() || lower_set(&keys[..rest.len()]) != lower_set(&rest) {
            return None;
        }
        let has_range = keys.len() >= rest.len() + range.len()
            && range.iter().zip(&keys[rest.len()..]).all(|(a, b)| a.eq_ignore_ascii_case(b));
        Some(ParameterizedFilter {
            index: i,
            keys: keys[..rest.len()].iter().chain(&range).cloned().collect(),
            missing_range: if has_range { Vec::new() } else { range },
            conditions,
        })
    })
}

/// `([is_deleted]=(0))`, `([status]='open' AND [kind]=(2))`: a filter made only of column-equals-literal conditions,
/// as (column, literal). `None` for any other filter.
pub(super) fn filter_equalities(filter: &str) -> Option<Vec<(String, String)>> {
    let plain: String = filter.chars().filter(|c| !matches!(c, '[' | ']' | '"' | '(' | ')')).collect();
    let spaced = plain.replace('=', " = ");
    let words: Vec<&str> = spaced.split_whitespace().collect();
    words
        .split(|w| w.eq_ignore_ascii_case("and"))
        .map(|condition| match condition {
            [column, "=", literal] if is_literal(literal) => {
                Some((column.rsplit('.').next().unwrap_or(column).to_string(), literal.to_string()))
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .filter(|c| !c.is_empty())
}

pub(crate) fn is_literal(word: &str) -> bool {
    word.parse::<f64>().is_ok() || (word.ends_with('\'') && (word.starts_with('\'') || word.starts_with("N'")))
}

/// Whether `text` compares `table`'s `column` to a parameter: `[column] = @p`, `t.column = @p` or `@p = column`.
pub(super) fn compares_to_parameter(text: &str, table: &str, column: &str) -> bool {
    compared_to(text, table, column, |value| value.starts_with('@') && value.len() > 1)
}

/// Whether `a` and `b` are the same literal: `0` and `(0)`, `'open'` and `N'open'`.
pub(super) fn same_literal(a: &str, b: &str) -> bool {
    let plain = |v: &str| {
        let v = v.trim().trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace());
        v.strip_prefix(['n', 'N']).filter(|rest| rest.starts_with('\'')).unwrap_or(v).to_ascii_lowercase()
    };
    let (a, b) = (plain(a), plain(b));
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => !a.is_empty() && a == b,
    }
}

/// Whether `text` compares `table`'s `column` with `=` to a value `wanted` accepts, on either side, the column perhaps
/// bracketed or qualified: `[column] = @p`, `t.column = 0`, `N'open' = column`. A column qualified by another table's
/// name or alias (`[app].[call_sessions].[is_deleted] = 0` in a join) is that table's, not this one's. Text is compared
/// lower-cased, its aliases read as SQL Server writes them (the optimizer's requests and Azure's are its own).
pub(crate) fn compared_to(text: &str, table: &str, column: &str, wanted: impl Fn(&str) -> bool) -> bool {
    let plain = text.chars().filter(|c| !matches!(c, '[' | ']' | '"')).collect::<String>().to_ascii_lowercase();
    let column = column.to_ascii_lowercase();
    let table = table.to_ascii_lowercase();
    let aliases = culprit_core::sql::table_aliases(text, Engine::Mssql);
    let ours = |qualifier: &str| {
        qualifier == table
            || aliases.iter().any(|(alias, name)| alias == qualifier && name.rsplit('.').next() == Some(table.as_str()))
    };
    let part = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    plain.match_indices(&column).any(|(at, _)| {
        let end = at + column.len();
        if plain[..at].chars().next_back().is_some_and(part) || plain[end..].chars().next().is_some_and(part) {
            return false;
        }
        // The column perhaps qualified: by this table, or an alias the statement gives it.
        let mut before = &plain[..at];
        if let Some(qualified) = before.strip_suffix('.') {
            let unqualified = qualified.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
            if !ours(&qualified[unqualified.len()..]) {
                return false;
            }
            before = unqualified;
        }
        // `column = value`
        if let Some(value) = plain[end..].trim_start().strip_prefix('=') {
            return wanted(operand_after(value));
        }
        // `value = column`
        before.trim_end().strip_suffix('=').is_some_and(|lhs| {
            // `<=`, `>=` and `!=` aren't equality.
            !lhs.ends_with(['<', '>', '!']) && wanted(operand_before(lhs))
        })
    })
}

/// The value right after `=`: a parameter, a number or a quoted string, past any opening parentheses.
pub(super) fn operand_after(text: &str) -> &str {
    let text = text.trim_start().trim_start_matches(|c: char| c == '(' || c.is_whitespace());
    let quote = if text.starts_with('\'') {
        Some(0)
    } else if text.starts_with("n'") {
        Some(1)
    } else {
        None
    };
    match quote {
        Some(open) => match text[open + 1..].find('\'') {
            Some(close) => &text[..open + 1 + close + 1],
            None => text,
        },
        None => {
            let end = text
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$' | '.' | '-' | '+')))
                .unwrap_or(text.len());
            &text[..end]
        }
    }
}

/// The value right before `=`, the same way.
pub(super) fn operand_before(text: &str) -> &str {
    let text = text.trim_end().trim_end_matches(|c: char| c == ')' || c.is_whitespace());
    if let Some(inner) = text.strip_suffix('\'') {
        let Some(open) = inner.rfind('\'') else { return text };
        let start = if text[..open].ends_with('n') { open - 1 } else { open };
        return &text[start..];
    }
    let start = text
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$' | '.' | '-' | '+')))
        .map_or(0, |p| p + 1);
    &text[start..]
}

/// The finding when an existing filtered index would serve the request but its queries pass the filter's column as a
/// parameter: the fix is the literal, and only what the index can't do after it (a range, included columns) is left.
pub(super) fn parameterized_filter_finding(
    snapshot: &Snapshot,
    mut f: Finding,
    c: &Candidate,
    linked: &[&QueryStat],
    miss: &ParameterizedFilter,
) -> Finding {
    let index = miss.index;
    let columns = miss.columns();
    let literals = miss.literals();
    let lookups = c.seeks + c.scans;
    let one = linked.len() == 1;
    let who = if one { format!("A query on {}", c.table) } else { format!("{} queries on {}", linked.len(), c.table) };
    let (passes, them) = if one { ("passes", "it") } else { ("pass", "them") };
    let below = if one { "the query below" } else { "the queries below" };
    let index_keys: Vec<String> = index.key_names().iter().map(|k| k.to_string()).collect();
    let equality = &miss.keys[..miss.keys.len() - miss.missing_range.len()];

    let mut include = c.include.clone();
    if let Some(note) = drop_large_includes(snapshot, &c.schema, &c.table, &mut include) {
        f.caveats.push(note);
    }
    if include.len() > MAX_INCLUDE {
        include.clear();
    }
    let remainder = miss.remainder(&include);
    let partial = !matches!(remainder, Remainder::Nothing);

    f.title = format!("{who} {passes} {} as a parameter, so {} can't serve {them}", format::list(&columns), index.name);
    let serves = if miss.missing_range.is_empty() {
        format!("already serves these lookups where {literals}")
    } else {
        format!(
            "already finds these rows where {literals} by {}, though not by {}",
            format::list(equality),
            format::list(&miss.missing_range)
        )
    };
    f.summary = format!(
        "SQL Server's optimizer asked for an index on {}.{} ({}) {} times {}, but {} ({}) {serves}. \
         It's a filtered index, and SQL Server uses one only for a query whose filter matches it as written: {below} {} {} to a parameter, \
         which could hold any value, so the optimizer can't use the index and asks for a new one instead.",
        c.schema,
        c.table,
        format::list(&c.keys()),
        format::count(lookups),
        since_phrase(snapshot),
        index.name,
        format::list(&index_keys),
        if one { "compares" } else { "compare" },
        format::list(&columns),
    );
    f.severity = Severity::Low;
    // Whether the rest is worth an index of its own is only known once the queries use the one there is.
    f.confidence = if matches!(remainder, Remainder::Index { .. }) { Confidence::Low } else { Confidence::Medium };
    f.impact.score = (4.0 * (c.weight + 1.0).log10()).clamp(2.0, 20.0);
    f.impact.label = format!("{} lookups", format::count(lookups));
    f.queries = query_refs(linked);
    f.metrics.push(Metric::new("Lookups that wanted it", lookups, Unit::Count));
    f.metrics.push(Metric::new("Estimated improvement", c.impact_pct / 100.0, Unit::Ratio));
    let lead = format!("Write the filter as the index does, {literals}, instead of a parameter in {below}");
    let (summary, sql) = match (&remainder, miss.remainder_sql(snapshot, &remainder)) {
        (Remainder::Index { include, .. }, Some((sql, _))) => {
            let replaces = index_keys.len() == equality.len()
                && !index.is_unique
                && !index.is_primary_key
                && index.include.iter().all(|i| include.iter().any(|c| c.eq_ignore_ascii_case(i)));
            (
                format!(
                    "{lead}: {} then finds {} rows by {}. If the range on {} still costs too much after that, create the index below: \
                     the request without {}, filtered the same way, so it holds only the rows where {literals}.{}",
                    index.name,
                    if one { "its" } else { "their" },
                    format::list(equality),
                    format::list(&miss.missing_range),
                    format::list(&columns),
                    if replaces {
                        format!(" It starts with {}'s keys, so once it's in use {} can go.", index.name, index.name)
                    } else {
                        String::new()
                    }
                ),
                Some(sql),
            )
        }
        (Remainder::Include(missing), Some((sql, _))) => (
            format!(
                "{lead}: {} then serves {them}. If reading {} from the table for each row still costs too much after that, \
                 include {} in {} (below: the rebuild keeps its filter).",
                index.name,
                format::list(missing),
                if missing.len() == 1 { "it" } else { "them" },
                index.name
            ),
            Some(sql),
        ),
        _ => (format!("{lead}: {} then serves {them}, and no new index is needed.", index.name), None),
    };
    // The write cost of "this index" only beside the SQL that creates it.
    let cost = if partial && sql.is_some() { write_cost(snapshot, &c.schema, &c.table) } else { None };
    f.fix = Some(Fix { summary, sql, cost });
    f.verify = vec![if partial {
        format!(
            "Change the query, let the workload run, then re-scan: the query's plan seeks {}, and if the optimizer still asks for an index, it's the one above.",
            index.name
        )
    } else {
        format!(
            "Change the query, let the workload run, then re-scan: this request disappears, and the query's plan seeks {}.",
            index.name
        )
    }];
    f.caveats.push(format!(
        "If {} also {} with other values for {}, those runs can't use {}: give them a statement of their own, with their own literal.",
        if one { "the query" } else { "the queries" },
        if one { "runs" } else { "run" },
        format::list(&columns),
        index.name
    ));
    f
}

/// SQL Server INCLUDE columns that hold large objects: (max) strings and binaries, text, ntext, image, xml, json. Including
/// one copies the column's data into the index (most of a table whose rows are documents), so the suggestions keep the
/// keys and let the few rows found go back to the table for it. Returns the sentence that says what was left out.
pub(crate) fn drop_large_includes(
    snapshot: &Snapshot,
    schema: &str,
    table: &str,
    include: &mut Vec<String>,
) -> Option<String> {
    // Only SQL Server suggests INCLUDE columns, and in PostgreSQL `text` is the everyday string type.
    if snapshot.server.engine != Engine::Mssql {
        return None;
    }
    let is_large = |name: &str| {
        snapshot
            .columns
            .iter()
            .find(|c| {
                c.schema.eq_ignore_ascii_case(schema)
                    && c.table.eq_ignore_ascii_case(table)
                    && c.name.eq_ignore_ascii_case(name)
            })
            .is_some_and(|c| {
                let t = c.data_type.to_ascii_lowercase();
                t.contains("(max)") || matches!(t.as_str(), "text" | "ntext" | "image" | "xml" | "json")
            })
    };
    let large: Vec<String> = include.iter().filter(|c| is_large(c)).cloned().collect();
    if large.is_empty() {
        return None;
    }
    include.retain(|c| !is_large(c));
    let one = large.len() == 1;
    let (it, they) = if one { ("it", "it") } else { ("them", "they") };
    // What the table keeps apart from its rows (LOB and row-overflow pages) says whether the large values are there; the
    // type alone says only that they can be.
    let apart = snapshot.table(schema, table).and_then(|t| match (t.lob_bytes, t.overflow_bytes) {
        (None, None) => None,
        (lob, overflow) => Some(lob.unwrap_or(0.0) + overflow.unwrap_or(0.0)),
    });
    Some(match apart {
        Some(bytes) if bytes > 0.0 => format!(
            "Left {} out of INCLUDE: {} type can hold large values, and {schema}.{table} keeps {} of large values apart \
             from its rows. Including {it} could copy that data into the index; the rows the index finds go back to the \
             table for {it}, which costs less.",
            format::list(&large),
            if one { "its" } else { "their" },
            format::bytes(bytes),
        ),
        _ => format!(
            "Left {} out of INCLUDE: {} type can hold large values{}, and including {it} would copy whatever {they} \
             {} into the index. The rows the index finds go back to the table for {it}.",
            format::list(&large),
            if one { "its" } else { "their" },
            if apart.is_some() {
                format!(", though {schema}.{table} keeps none apart from its rows now")
            } else {
                String::new()
            },
            if one { "holds" } else { "hold" },
        ),
    })
}

/// Inserts, updates and deletes a day on a table, from the clustered index's update counter (SQL Server) or the
/// table's row counters (Postgres, MySQL), over the days the counters have been accumulating.
pub(crate) fn writes_per_day(snapshot: &Snapshot, schema: &str, table: &str) -> Option<f64> {
    let base = snapshot.indexes_on(schema, table).find(|i| i.is_clustered || i.is_primary_key).and_then(|i| i.writes);
    let row_writes = snapshot
        .table(schema, table)
        .and_then(|t| Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0)));
    let writes = base.or(row_writes)?;
    // A rate a day only over 20 hours of counters: under that it would be an extrapolation.
    let days = counter_days(snapshot).filter(|d| per_day_is_measured(*d))?;
    Some(writes / days)
}
