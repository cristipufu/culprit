//! Missing-index candidate merging, existing index overlap/widening analysis, table size/history checks, and INCLUDE
//! trimming shared by the `missing-index` check and the index advisor.

use super::*;

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
            Some(_) => fits_index_filter(i, asking),
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
    let passed_over = |i: &&IndexStat| i.filter.is_some() && !fits_index_filter(i, asking);
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
    let filtered: Vec<String> = index_filter_equalities(i)
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
        .filter(|i| fits_index_filter(i, asking))
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
/// that ask for the rows it holds ([`fits_index_filter`]). The one that seeks on the most, unfiltered first, then the
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
        .filter(|i| fits_index_filter(i, asking))
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

/// Coverage established before redaction stays attached to the statement that supplied it.
pub(crate) fn fits_index_filter(index: &IndexStat, asking: &[&QueryStat]) -> bool {
    let Some(filter) = index.filter.as_deref() else {
        return true;
    };
    if let Some(facts) = &index.filter_facts {
        return !asking.is_empty() && asking.iter().all(|q| facts.matching_queries.contains(&q.id));
    }
    !asking.is_empty() && asking.iter().all(|q| culprit_core::index_filter::matches(&index.table, filter, &q.text))
}

pub(super) fn index_filter_equalities(index: &IndexStat) -> Option<Vec<(String, String)>> {
    match &index.filter_facts {
        Some(facts) => facts.equalities.clone(),
        None => index.filter.as_deref().and_then(filter_equalities),
    }
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
pub(super) const MEASURED_READS: f64 = 100.0;

/// The pages a run the queries behind a request read, over all their runs; `None` where the engine doesn't count them.
pub(super) fn reads_per_run(linked: &[&QueryStat]) -> Option<f64> {
    let runs: f64 = linked.iter().map(|q| q.executions).sum();
    let reads: f64 = linked.iter().map(|q| q.logical_reads).sum::<Option<f64>>()?;
    (runs > 0.0).then(|| reads / runs)
}

/// How much an optimizer request is worth: what the queries that asked for it would save (their time times its
/// estimated gain), or, with none of them in the window, how often it was asked for.
pub(super) fn request_severity(snapshot: &Snapshot, c: &Candidate, linked: &[&QueryStat]) -> Severity {
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
pub(super) fn too_small_finding(snapshot: &Snapshot, mut f: Finding, c: &Candidate, linked: &[&QueryStat]) -> Finding {
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
