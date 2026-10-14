use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, MissingIndex, QueryStat,
    Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::support::*;

const MAX_INCLUDE: usize = 6;

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
            Some(filter) => fits_filter(filter, asking),
        })
        .min_by_key(|i| i.is_disabled)
}

/// A filtered index with the suggestion's keys that [`existing_match`] passed over: the queries don't ask for only the
/// rows it holds, or Culprit can't show that they do.
fn filtered_bystander<'a>(snapshot: &'a Snapshot, c: &Candidate, asking: &[&QueryStat]) -> Option<&'a IndexStat> {
    same_keys_on_table(snapshot, c).find(|i| i.filter.as_deref().is_some_and(|f| !fits_filter(f, asking)))
}

/// The indexes on the suggestion's table whose leading keys are its equality columns (in any order), then its range.
fn same_keys_on_table<'a>(snapshot: &'a Snapshot, c: &Candidate) -> impl Iterator<Item = &'a IndexStat> {
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

/// Every query in `asking` (at least one) compares each of the filter's columns to the filter's own literal.
fn fits_filter(filter: &str, asking: &[&QueryStat]) -> bool {
    let Some(conditions) = filter_equalities(filter) else { return false };
    !asking.is_empty()
        && asking.iter().all(|q| {
            conditions
                .iter()
                .all(|(column, literal)| compared_to(&q.text, column, |value| same_literal(value, literal)))
        })
}

/// A filter as people read it: "is_deleted = 0", or the definition without its brackets.
fn readable_filter(filter: &str) -> String {
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

fn missing_index_finding(snapshot: &Snapshot, queries: &[QueryStat], mut c: Candidate) -> Finding {
    let keys = c.keys();
    let table_name = format!("{}.{}", c.schema, c.table);
    let key_text = format::list(&keys);
    let mut f = finding(
        "missing-index",
        &format!("{table_name}({})", keys.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    let linked = asking_queries(queries, &c);
    if let Some(miss) = parameterized_filter(snapshot, &c, &linked) {
        return parameterized_filter_finding(snapshot, f, &c, &linked, &miss);
    }
    let table = snapshot.table(&c.schema, &c.table);
    let rows = table.and_then(|t| t.rows);
    let lookups = c.seeks + c.scans;

    f.title = format!("{} is searched by {} without a supporting index", c.table, key_text);
    let mut summary = format!(
        "SQL Server's optimizer asked for an index on {} ({}) {} times {}. Without it, each of those lookups reads more of {}{} than it needs to. It estimates the index would make them about {:.0}% cheaper.",
        table_name,
        key_text,
        format::count(lookups),
        since_phrase(snapshot),
        table_name,
        rows.map(|r| format!(" ({} rows)", format::count(r))).unwrap_or_default(),
        c.impact_pct
    );
    match query_impact(snapshot, &linked) {
        Some(impact) => {
            summary.push_str(&format!(
                " The {} that asked for it {} for {} of database time.",
                if linked.len() == 1 { "query".to_string() } else { format!("{} queries", linked.len()) },
                if linked.len() == 1 { "accounts" } else { "account" },
                format::percent(impact.share)
            ));
            apply_query_impact(&mut f, &impact);
            // What the index saves is the queries' time times the optimizer's estimate of the gain: 11% of a query at
            // 0.01% of database time is nothing to rank or raise above Low.
            let gain = (c.impact_pct / 100.0).clamp(0.0, 1.0);
            let time_ms: f64 = linked.iter().map(|q| q.total_time_ms).sum();
            f.impact.score = score_for_share(impact.share * gain);
            f.severity = severity_for_time(snapshot, time_ms * gain, impact.share * gain);
            f.confidence = if c.impact_pct >= 50.0 { Confidence::High } else { Confidence::Medium };
        }
        None if linked.is_empty() && snapshot.workload.is_some() => {
            summary.push_str(
                " None of the queries in this scan asked for it: the requests came from plans that ran before the scan's window, or that Query Store no longer keeps.",
            );
            f.severity = if c.impact_pct >= 70.0 && lookups >= 1000.0 { Severity::Medium } else { Severity::Low };
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
        None => {
            f.severity = if c.impact_pct >= 70.0 && lookups >= 1000.0 { Severity::Medium } else { Severity::Low };
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

    if let Some((last, window)) = wanted_before_window(snapshot, &c) {
        f.inactive = true;
        f.severity = Severity::Low;
        f.impact.score *= 0.25;
        f.last_seen = Some(last.clone());
        f.caveats.insert(
            0,
            format!(
                "SQL Server's optimizer last asked for this index on {}, before {window}: the queries that wanted it have \
                 most likely changed or stopped running. Its requests are kept until the server restarts.",
                format::friendly_date(&last, &snapshot.taken_at),
            ),
        );
    }

    f.fix = Some(candidate_fix(snapshot, &c, &linked).fix);
    f.verify = vec![
        "Create the index, let the workload run, then re-scan: this suggestion should disappear.".into(),
        "The queries listed here should show an Index Seek on the new index and far fewer reads per execution.".into(),
    ];
    f.caveats.push("Missing-index suggestions are per-query and ignore the cost of maintaining the index; review overlapping suggestions together.".into());
    f
}

/// When SQL Server's optimizer last wanted the index, when that is before the scan's window (or, without query
/// history, before the last week: a weekly job has had its chance), and the window as words: none of the window's
/// queries wanted it. `None` when one of the merged requests was wanted within it, or when it isn't known when.
fn wanted_before_window(snapshot: &Snapshot, c: &Candidate) -> Option<(String, String)> {
    let (start, window) = match &snapshot.workload {
        Some(w) => {
            let end = w.window_end.as_deref().unwrap_or(&snapshot.taken_at);
            (parse_epoch_seconds(end)? - w.window_hours * 3600.0, w.window_phrase())
        }
        None => (parse_epoch_seconds(&snapshot.taken_at)? - MIN_INDEX_DAYS * 86_400.0, "the last week".to_string()),
    };
    let mut latest: Option<(f64, &str)> = None;
    for m in &c.requests {
        let text = m.last_used.as_deref()?;
        let at = parse_epoch_seconds(text)?;
        if latest.is_none_or(|(l, _)| at > l) {
            latest = Some((at, text));
        }
    }
    let (at, text) = latest?;
    (at < start).then(|| (text.to_string(), window))
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
    let create = |why: Option<String>| Fix {
        summary: format!(
            "{}Create an index on {}{}. Equality columns come first; put the most selective one at the front if you know it.{}",
            why.map(|w| format!("{w} ")).unwrap_or_default(),
            format::list(&keys),
            if c.include.is_empty() { String::new() } else { format!(", including {}", format::list(&c.include)) },
            filtered_bystander(snapshot, c, linked)
                .map(|i| {
                    let rows = format!(
                        " {} has these keys but holds only the rows where {}",
                        i.name,
                        readable_filter(i.filter.as_deref().unwrap_or_default())
                    );
                    match linked.len() {
                        0 => format!("{rows}, and Culprit can't show that these lookups ask for only those."),
                        1 => format!("{rows}, which the query below doesn't ask for."),
                        _ => format!("{rows}, which the queries below don't ask for."),
                    }
                })
                .unwrap_or_default()
        ),
        sql: Some(create_index_sql(snapshot, &c.schema, &c.table, &keys, &c.include)),
        cost: write_cost(snapshot, &c.schema, &c.table),
    };
    let Some(existing) = existing_match(snapshot, c, linked) else {
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

fn plan_asks_for(q: &QueryStat, c: &Candidate) -> bool {
    q.plan.as_ref().is_some_and(|p| {
        p.index_hints.iter().any(|h| {
            let table_ok = h.table.to_ascii_lowercase().replace(['[', ']'], "").ends_with(&format!(
                "{}.{}",
                c.schema.to_ascii_lowercase(),
                c.table.to_ascii_lowercase()
            ));
            table_ok && lower_set(&h.equality) == lower_set(&c.equality)
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
                && asking.iter().any(|q| compares_to_parameter(&q.text, column))
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
fn filter_equalities(filter: &str) -> Option<Vec<(String, String)>> {
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

fn is_literal(word: &str) -> bool {
    word.parse::<f64>().is_ok() || (word.ends_with('\'') && (word.starts_with('\'') || word.starts_with("N'")))
}

/// Whether `text` compares `column` to a parameter: `[column] = @p`, `t.column = @p` or `@p = column`.
fn compares_to_parameter(text: &str, column: &str) -> bool {
    compared_to(text, column, |value| value.starts_with('@') && value.len() > 1)
}

/// Whether `a` and `b` are the same literal: `0` and `(0)`, `'open'` and `N'open'`.
fn same_literal(a: &str, b: &str) -> bool {
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

/// Whether `text` compares `column` with `=` to a value `wanted` accepts, on either side, the column perhaps bracketed
/// or qualified: `[column] = @p`, `t.column = 0`, `N'open' = column`. Text is compared lower-cased.
fn compared_to(text: &str, column: &str, wanted: impl Fn(&str) -> bool) -> bool {
    let plain = text.chars().filter(|c| !matches!(c, '[' | ']' | '"')).collect::<String>().to_ascii_lowercase();
    let column = column.to_ascii_lowercase();
    let part = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    plain.match_indices(&column).any(|(at, _)| {
        let end = at + column.len();
        if plain[..at].chars().next_back().is_some_and(part) || plain[end..].chars().next().is_some_and(part) {
            return false;
        }
        // `column = value`
        if let Some(value) = plain[end..].trim_start().strip_prefix('=') {
            return wanted(operand_after(value));
        }
        // `value = column`, the column perhaps qualified
        let mut before = &plain[..at];
        if let Some(qualified) = before.strip_suffix('.') {
            before = qualified.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
        }
        before.trim_end().strip_suffix('=').is_some_and(|lhs| {
            // `<=`, `>=` and `!=` aren't equality.
            !lhs.ends_with(['<', '>', '!']) && wanted(operand_before(lhs))
        })
    })
}

/// The value right after `=`: a parameter, a number or a quoted string, past any opening parentheses.
fn operand_after(text: &str) -> &str {
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
fn operand_before(text: &str) -> &str {
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
fn parameterized_filter_finding(
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
    f.fix = Some(Fix { summary, sql, cost: if partial { write_cost(snapshot, &c.schema, &c.table) } else { None } });
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
    let size = snapshot
        .table(schema, table)
        .and_then(|t| t.data_bytes)
        .map(|b| format!(" ({schema}.{table} holds {})", format::bytes(b)))
        .unwrap_or_default();
    Some(format!(
        "Left {} out of INCLUDE: {} large values, and including {} would copy that data into the index{size}. The rows the \
         index finds go back to the table for {}, which costs less.",
        format::list(&large),
        if large.len() == 1 { "it holds" } else { "they hold" },
        if large.len() == 1 { "it" } else { "them" },
        if large.len() == 1 { "it" } else { "them" },
    ))
}

/// What [`widen_index`] can do with an existing index.
pub(crate) enum Widening {
    /// A statement that changes only what was asked (columns included, or the index brought back), with the sentence
    /// that says so and what it costs.
    Sql(Fix),
    /// No statement can do it in place without changing what the index is; the sentence says why, so the caller can
    /// suggest a separate index instead.
    Refused(String),
}

/// The one way Culprit widens an existing index: rebuild it with `include` added to its included columns, keeping
/// everything else it is (unique or not, its filter, its keys in their order and direction, the columns it already
/// includes, and ONLINE where the edition has it), or bring it back when it is disabled. Fix SQL must never damage what
/// exists, so it refuses rather than guess:
///
/// - a clustered index or a primary key can't take included columns (SQL Server refuses to change a constraint's index
///   with DROP_EXISTING, and the clustered index already holds every column);
/// - only SQL Server rebuilds an index with new included columns in place (PostgreSQL would need a new index and
///   MySQL has no INCLUDE).
///
/// A disabled index (SQL Server DISABLE, a PostgreSQL index left invalid by a failed build, a MySQL invisible or
/// MariaDB ignored one) is brought back by a rebuild, or made visible again; on SQL Server the rebuild includes
/// `include` when it can. Used by the missing-index finding, the index advisor and the filtered-scan finding; a plan
/// insight that wants an index to cover a lookup can use it too.
pub(crate) fn widen_index(snapshot: &Snapshot, existing: &IndexStat, include: &[String]) -> Widening {
    let engine = snapshot.server.engine;
    let target = qualified(engine, &existing.schema, &existing.table);
    let name = quote(engine, &existing.name);
    let add = lacking_includes(existing, include);
    let can_include = engine == Engine::Mssql && !existing.is_clustered && !existing.is_primary_key;
    if existing.is_disabled && (add.is_empty() || !can_include) {
        return Widening::Sql(bring_back(snapshot, existing, &add));
    }
    if add.is_empty() {
        return Widening::Refused(format!(
            "{} already has these keys and includes {}.",
            existing.name,
            format::list(include)
        ));
    }
    if existing.is_clustered {
        return Widening::Refused(format!(
            "{} is the table's clustered index: it already holds every column, so there is nothing to include in it.",
            existing.name
        ));
    }
    if existing.is_primary_key {
        return Widening::Refused(format!(
            "{} enforces the primary key, and SQL Server won't change a constraint's index to include columns.",
            existing.name
        ));
    }
    if engine != Engine::Mssql {
        return Widening::Refused(match engine {
            Engine::Postgres => format!(
                "PostgreSQL can't add included columns to {} in place; a new index has to be built.",
                existing.name
            ),
            _ => format!("{} has no included columns: an index there holds only its keys.", snapshot.server.name()),
        });
    }
    let keys: Vec<String> = existing
        .columns
        .iter()
        .map(|k| match k.strip_suffix(" DESC") {
            Some(column) => format!("{} DESC", quote(engine, column)),
            None => quote(engine, k),
        })
        .collect();
    let all_include: Vec<String> = existing.include.iter().chain(&add).map(|c| quote(engine, c)).collect();
    let mut options = vec!["DROP_EXISTING = ON"];
    if supports_online_index(snapshot) {
        options.push("ONLINE = ON");
    }
    // The rebuild replaces the definition whole: without UNIQUE a unique index would stop enforcing it, and without its
    // filter a filtered index would take every row.
    let filter = existing.filter.as_deref().map(|f| format!("\n    WHERE {f}")).unwrap_or_default();
    let sql = format!(
        "CREATE {}NONCLUSTERED INDEX {name}\n    ON {target} ({})\n    INCLUDE ({}){filter}\n    WITH ({});",
        if existing.is_unique { "UNIQUE " } else { "" },
        keys.join(", "),
        all_include.join(", "),
        options.join(", ")
    );
    let mut summary = if existing.is_disabled {
        format!(
            "{} has the right keys but is disabled, so SQL Server neither maintains nor reads it. Rebuilding it brings it \
             back, with {} included so it covers the query; if it was disabled because nothing read it, the queries \
             below now would.",
            existing.name,
            format::list(&add)
        )
    } else {
        format!(
            "{} already has the right keys but makes every matching row go back to the table for {}. Rebuild it with \
             those columns included so it covers the query.",
            existing.name,
            format::list(&add)
        )
    };
    summary.push_str(
        " The statement keeps its keys, their order, its filter and the columns it already includes; restate \
         DATA_COMPRESSION, FILLFACTOR or its partition scheme in it if the index uses them.",
    );
    if existing.is_unique {
        summary.push_str(&format!(
            " It stays unique. If {} enforces a UNIQUE constraint rather than being a unique index, SQL Server refuses \
             the rebuild (error 1907): create a separate index with these keys and included columns instead.",
            existing.name
        ));
    }
    Widening::Sql(Fix {
        summary,
        sql: Some(sql),
        cost: existing.size_bytes.map(|b| {
            format!("The index grows from {}; writes that touch the included columns maintain it.", format::bytes(b))
        }),
    })
}

/// The columns of `include` that `existing` holds neither as a key nor as an included column, once each.
fn lacking_includes(existing: &IndexStat, include: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in include {
        let held = existing.include.iter().any(|i| i.eq_ignore_ascii_case(c))
            || existing.key_names().iter().any(|k| k.eq_ignore_ascii_case(c));
        if !held && !out.iter().any(|o| o.eq_ignore_ascii_case(c)) {
            out.push(c.clone());
        }
    }
    out
}

/// A disabled index brought back as it was: SQL Server rebuilds it, PostgreSQL rebuilds an invalid one, MySQL makes an
/// invisible one visible and MariaDB stops ignoring it. `wanted` are columns a request also wanted included that this
/// can't add.
fn bring_back(snapshot: &Snapshot, existing: &IndexStat, wanted: &[String]) -> Fix {
    let engine = snapshot.server.engine;
    let target = qualified(engine, &existing.schema, &existing.table);
    let name = quote(engine, &existing.name);
    let (sql, how) = match engine {
        Engine::Mssql => {
            // SQL Server can't rebuild a disabled clustered index online.
            let online = supports_online_index(snapshot) && !existing.is_clustered;
            (
                format!("ALTER INDEX {name} ON {target} REBUILD{};", if online { " WITH (ONLINE = ON)" } else { "" }),
                "it is disabled, so SQL Server neither maintains nor reads it. Rebuilding it brings it back as it was",
            )
        }
        Engine::Postgres if culprit_core::version_at_least(&snapshot.server.version, [12, 0, 0]) => (
            format!("REINDEX INDEX CONCURRENTLY {};", qualified(engine, &existing.schema, &existing.name)),
            "it is invalid (a build that didn't finish), so PostgreSQL never reads it. Rebuilding it makes it usable",
        ),
        Engine::Postgres => (
            format!("REINDEX INDEX {};", qualified(engine, &existing.schema, &existing.name)),
            "it is invalid (a build that didn't finish), so PostgreSQL never reads it. Rebuilding it makes it usable \
             (this version can't rebuild concurrently: writes to the table wait meanwhile)",
        ),
        Engine::Mysql if snapshot.server.name() == "MariaDB" => (
            format!("ALTER TABLE {target} ALTER INDEX {name} NOT IGNORED;"),
            "the optimizer is told to ignore it. Letting it use the index again is instant",
        ),
        Engine::Mysql => (
            format!("ALTER TABLE {target} ALTER INDEX {name} VISIBLE;"),
            "it is invisible to the optimizer. Making it visible again is instant",
        ),
    };
    let mut summary = format!(
        "{} has these keys, but {how}; if it was set aside because nothing read it, the queries below now would.",
        existing.name
    );
    if !wanted.is_empty() {
        summary.push_str(&format!(
            " It doesn't include {}; once it's back, the optimizer says whether it still wants {}.",
            format::list(wanted),
            if wanted.len() == 1 { "it" } else { "them" }
        ));
    }
    Fix { summary, sql: Some(sql), cost: None }
}

/// [`widen_index`] as a fix: the statement, or, where none is safe, the sentence that says why and no SQL.
pub(crate) fn widen_fix(snapshot: &Snapshot, existing: &IndexStat, include: &[String]) -> Fix {
    match widen_index(snapshot, existing, include) {
        Widening::Sql(fix) => fix,
        Widening::Refused(why) => Fix { summary: why, sql: None, cost: None },
    }
}

pub(crate) fn write_cost(snapshot: &Snapshot, schema: &str, table: &str) -> Option<String> {
    let (writes, days) = measured_writes(snapshot, schema, table)?;
    if writes <= 0.0 {
        return Some(format!(
            "{schema}.{table} received no writes, so this index would cost nothing to keep up there."
        ));
    }
    Some(format!(
        "{schema}.{table} receives {}; each one will also update this index.",
        how_many(snapshot, writes, days, "write", "writes")
    ))
}

/// Inserts, updates and deletes on a table since its counters started, and the days they cover (at least half an
/// hour, so a just-restarted server doesn't read as a write storm).
pub(crate) fn measured_writes(snapshot: &Snapshot, schema: &str, table: &str) -> Option<(f64, f64)> {
    let base = snapshot.indexes_on(schema, table).find(|i| i.is_clustered || i.is_primary_key).and_then(|i| i.writes);
    let row_writes = snapshot
        .table(schema, table)
        .and_then(|t| Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0)));
    let writes = base.or(row_writes)?;
    let days = counter_days_for_rate(snapshot)?;
    Some((writes, days))
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

pub fn unused_index_info() -> CheckInfo {
    CheckInfo {
        id: "unused-index".into(),
        title: "Unused indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::IndexUsage],
        description:
            "Indexes that are maintained on every write but no query has read since usage counters were reset. \
            One that may be less than a week old (a scan saw it appear, or its table changed before Culprit's first \
            scan) isn't called unused yet."
                .into(),
    }
}

pub fn unused_index(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let days = counter_days(snapshot);
    let mut by_table: Vec<(String, String, Vec<&IndexStat>)> = Vec::new();
    let redundant: Vec<&str> = redundant_indexes(snapshot).iter().map(|(a, _)| a.name.as_str()).collect();
    let mysql = snapshot.server.engine == Engine::Mysql;
    // InnoDB refuses to drop the index a foreign key relies on.
    for i in snapshot
        .indexes
        .iter()
        .filter(|i| is_unused(snapshot, i) && !redundant.contains(&i.name.as_str()))
        .filter(|i| !(mysql && backs_foreign_key(snapshot, i).is_some()))
    {
        match by_table.iter_mut().find(|(s, t, _)| *s == i.schema && *t == i.table) {
            Some((_, _, list)) => list.push(i),
            None => by_table.push((i.schema.clone(), i.table.clone(), vec![i])),
        }
    }
    by_table
        .into_iter()
        .map(|(schema, table, list)| {
            let table_name = format!("{schema}.{table}");
            let total_size: f64 = list.iter().filter_map(|i| i.size_bytes).sum();
            let writes = table_writes(snapshot, &schema, &table, &list);
            // An index that may be younger than the counters has gone unread only since it may have been created.
            let youngest = list
                .iter()
                .filter_map(|i| Some((index_age_days(snapshot, i)?, i.created_at_latest.as_deref()?)))
                .filter(|(age, _)| days.is_none_or(|d| *age < d))
                .min_by(|a, b| a.0.total_cmp(&b.0));
            let days = youngest.map(|(age, _)| age).or(days);
            let since = match youngest {
                Some((age, created)) => format!(
                    "in the {} days since {}, before which {} may not have existed",
                    age.round(),
                    format::friendly_date(created, &snapshot.taken_at),
                    if list.len() == 1 { "it" } else { "some of them" }
                ),
                None => since_phrase(snapshot),
            };
            let mut f = finding("unused-index", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
            f.title = match list.as_slice() {
                [one] => format!("{} on {} is never read", one.name, table),
                many => format!("{} indexes on {} are never read", many.len(), table),
            };
            let names = format::list(&list.iter().map(|i| i.name.clone()).collect::<Vec<_>>());
            f.summary = match writes {
                // Nothing written either: the cost is the space, not the upkeep.
                Some(0.0) => format!(
                    "No query has read {} {}, and {} hasn't been written to either. {} {} of storage and buffer memory.",
                    names,
                    since,
                    table_name,
                    if list.len() == 1 { "It takes" } else { "Together they take" },
                    format::bytes(total_size)
                ),
                _ => format!(
                    "No query has read {} {}{}. {} still has to be updated on every insert, and on updates that touch its columns, and it takes {} of storage and buffer memory.",
                    names,
                    since,
                    writes.map(|w| format!(", while {} was written {} times", table_name, format::count(w))).unwrap_or_default(),
                    if list.len() == 1 { "It" } else { "Each one" },
                    format::bytes(total_size)
                ),
            };
            // What the index costs: its size, or the upkeep of a busy table, as a rate (a total since a restart months
            // ago says nothing about today).
            let writes_a_day = writes.and_then(|w| counter_rate_per_day(snapshot, w));
            f.severity = if total_size >= 1024.0 * 1024.0 * 1024.0 || writes_a_day.is_some_and(|w| w >= UPKEEP_A_DAY) {
                Severity::Medium
            } else {
                Severity::Low
            };
            f.impact.score = base_score(f.severity) + (total_size / (1024.0 * 1024.0 * 1024.0)).min(10.0);
            f.impact.label = format!("{} unused", format::bytes(total_size));
            f.confidence = match days {
                Some(d) if d >= 30.0 => Confidence::High,
                Some(d) if d >= 7.0 => Confidence::Medium,
                _ => Confidence::Low,
            };
            f.metrics.push(Metric::new("Size", total_size, Unit::Bytes));
            if let Some(w) = writes {
                f.metrics.push(Metric::new("Writes to the table", w, Unit::Count));
            }
            if let Some(d) = days {
                f.metrics.push(Metric::new("Counting for", d, Unit::Days));
            }
            let sql = list
                .iter()
                .map(|i| match engine {
                    Engine::Mssql => format!(
                        "ALTER INDEX {} ON {} DISABLE;",
                        quote(engine, &i.name),
                        qualified(engine, &i.schema, &i.table)
                    ),
                    Engine::Postgres => drop_index_sql(engine, &i.schema, &i.table, &i.name),
                    Engine::Mysql => crate::index_health::hide_index_sql(&snapshot.server, i),
                })
                .collect::<Vec<_>>()
                .join("\n");
            f.fix = Some(Fix {
                summary: match engine {
                    Engine::Mssql => "Disable first: the definition stays, nothing reads it, and `ALTER INDEX … REBUILD` brings it back. Drop it after a full business cycle without complaints.".into(),
                    Engine::Postgres => "Drop it concurrently so writes aren't blocked. Save the definition first so you can recreate it.".into(),
                    Engine::Mysql if snapshot.server.name() == "MariaDB" && crate::index_health::mariadb_can_ignore(&snapshot.server) => "Have the optimizer ignore it first: it stays maintained, and `ALTER INDEX … NOT IGNORED` brings it back instantly. Drop it after a full business cycle without complaints.".into(),
                    Engine::Mysql if snapshot.server.name() == "MariaDB" => "Save its definition, then drop it: this MariaDB version can't hide an index (ignored indexes arrived in 10.6).".into(),
                    Engine::Mysql if !culprit_core::version_at_least(&snapshot.server.version, [8, 0, 0]) => "Save its definition, then drop it: MySQL 5.7 can't hide an index (invisible indexes arrived in 8.0).".into(),
                    Engine::Mysql => "Make it invisible first: the optimizer stops using it but it stays maintained, and `ALTER INDEX … VISIBLE` brings it back instantly. Drop it after a full business cycle without complaints.".into(),
                },
                sql: Some(sql),
                cost: Some(format!("Frees {} and makes writes to {} cheaper.", format::bytes(total_size), table_name)),
            });
            f.verify = vec!["Watch the queries on this table for a week after disabling; none should get slower.".into()];
            f.caveats.push("Usage counters reset when the server restarts or fails over, so an index used only by a monthly job can look unused.".into());
            f.caveats.push("Reads on read replicas or secondaries are not counted here. Check them before dropping.".into());
            let fk_backers: Vec<String> = list
                .iter()
                .filter_map(|i| backs_foreign_key(snapshot, i).map(|fk| format!("{} (foreign key {})", i.name, fk.name)))
                .collect();
            if !fk_backers.is_empty() {
                f.caveats.push(format!(
                    "{} also {} a foreign key: without it, deleting or updating parent rows scans this table.",
                    format::list(&fk_backers),
                    if fk_backers.len() == 1 { "supports" } else { "support" }
                ));
            }
            if let Some(defs) = definitions(&list) {
                f.caveats.push(format!("Definitions, to recreate if needed:\n{defs}"));
            }
            f
        })
        .collect()
}

/// Writes a day to a table that make keeping an unread index up to date a Medium problem.
const UPKEEP_A_DAY: f64 = 100_000.0;

/// The foreign key whose columns this index leads with, if any: dropping it would leave that foreign key unindexed.
pub(crate) fn backs_foreign_key<'a>(snapshot: &'a Snapshot, index: &IndexStat) -> Option<&'a culprit_core::ForeignKey> {
    let keys: Vec<String> = index.key_names().iter().map(|k| k.to_ascii_lowercase()).collect();
    snapshot.foreign_keys.iter().find(|fk| {
        fk.schema.eq_ignore_ascii_case(&index.schema)
            && fk.table.eq_ignore_ascii_case(&index.table)
            && !fk.columns.is_empty()
            && keys.len() >= fk.columns.len()
            && lower_set(&keys[..fk.columns.len()]) == lower_set(&fk.columns)
    })
}

/// Never read since the counters started, and not one the database needs for something else: a key, a constraint or
/// the table's storage. A filtered (partial) index counts: unread, it is only upkeep on every write it matches. No
/// reads says something only after a week of watching: an index that may be younger than that, or counters that started
/// less than a week ago (a restart, a failover, a statistics reset), haven't seen a weekly job run yet.
pub(crate) fn is_unused(snapshot: &Snapshot, i: &IndexStat) -> bool {
    let is_candidate = !i.is_primary_key && !i.is_unique && !i.is_clustered && !i.is_disabled && !is_partition_copy(i);
    is_candidate
        && i.reads == Some(0.0)
        && i.size_bytes.unwrap_or(0.0) >= 64.0 * 1024.0
        && !too_new(snapshot, i)
        && watched_long_enough(snapshot)
}

/// Days an index, and the counters watching it, must have existed before no reads says anything about it.
pub(crate) const MIN_INDEX_DAYS: f64 = 7.0;

/// Whether the usage counters have counted for a week ([`MIN_INDEX_DAYS`]). Unknown counts as not.
pub(crate) fn watched_long_enough(snapshot: &Snapshot) -> bool {
    counter_days(snapshot).is_some_and(|d| d >= MIN_INDEX_DAYS)
}

/// Days since the latest it may have been created, when a scan found reason to think it may be new.
fn index_age_days(snapshot: &Snapshot, i: &IndexStat) -> Option<f64> {
    days_between(i.created_at_latest.as_deref()?, &snapshot.taken_at).map(|d| d.max(0.0))
}

/// It may have been created less than a week ago, so its usage counters say little yet.
pub(crate) fn too_new(snapshot: &Snapshot, i: &IndexStat) -> bool {
    index_age_days(snapshot, i).is_some_and(|d| d < MIN_INDEX_DAYS)
}

fn table_writes(snapshot: &Snapshot, schema: &str, table: &str, list: &[&IndexStat]) -> Option<f64> {
    if let Some(w) = list.iter().filter_map(|i| i.writes).reduce(f64::max) {
        return Some(w);
    }
    let t = snapshot.table(schema, table)?;
    Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0))
}

fn definitions(list: &[&IndexStat]) -> Option<String> {
    let defs: Vec<String> = list.iter().filter_map(|i| i.definition.clone()).collect();
    (!defs.is_empty()).then(|| defs.join(";\n") + ";")
}

pub fn duplicate_index_info() -> CheckInfo {
    CheckInfo {
        id: "duplicate-index".into(),
        title: "Duplicate and redundant indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Indexes with the same keys as another index, or whose keys are a leading part of a wider one."
            .into(),
    }
}

pub fn duplicate_index(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let mut out = Vec::new();
    for (a, b) in redundant_indexes(snapshot) {
        let exact = same_keys(a, b);
        let table_name = format!("{}.{}", a.schema, a.table);
        let mut f = finding(
            "duplicate-index",
            &format!("{}.{}", table_name, a.name),
            Category::Indexes,
            subject(SubjectKind::Index, format!("{}.{}", table_name, a.name)),
        );
        f.title = if exact {
            format!("{} duplicates {} on {}", a.name, b.name, a.table)
        } else {
            format!("{} is covered by the wider {} on {}", a.name, b.name, a.table)
        };
        f.summary = format!(
            "{} has keys ({}) and {} has keys ({}). {} Every write to {} maintains both.",
            a.name,
            a.columns.join(", "),
            b.name,
            b.columns.join(", "),
            if exact {
                "They are the same index twice.".to_string()
            } else {
                format!("Any query that can seek on {} can seek on {} instead.", a.name, b.name)
            },
            table_name
        );
        let size = a.size_bytes.unwrap_or(0.0);
        f.severity = if size >= 512.0 * 1024.0 * 1024.0 { Severity::Medium } else { Severity::Low };
        f.impact.score = base_score(f.severity) + (size / (1024.0 * 1024.0 * 1024.0)).min(8.0);
        f.impact.label = format!("{} redundant", format::bytes(size));
        f.confidence = if exact { Confidence::High } else { Confidence::Medium };
        f.metrics.push(Metric::new("Size", size, Unit::Bytes));
        if let Some(r) = a.reads {
            f.metrics.push(Metric::new(format!("Reads of {}", a.name), r, Unit::Count));
        }
        f.fix = Some(Fix {
            summary: format!("Drop {} and keep {}.", a.name, b.name),
            sql: Some(drop_index_sql(engine, &a.schema, &a.table, &a.name)),
            cost: Some(format!("Frees {}.", format::bytes(size))),
        });
        if !exact {
            f.caveats.push(format!(
                "{} is narrower, so scans of it read fewer pages. Keep it if a hot query scans it rather than seeking.",
                a.name
            ));
        }
        out.push(f);
    }
    out
}

pub(crate) fn same_keys(a: &IndexStat, b: &IndexStat) -> bool {
    a.columns.len() == b.columns.len()
        && a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y))
        && same_opclasses(a, b, a.columns.len())
}

/// Whether the first `n` keys of `a` and `b` use the same operator class and collation. On the same column,
/// `text_pattern_ops` (or `COLLATE "C"`) serves `LIKE 'abc%'` and the default serves ORDER BY and range reads: two
/// indexes that differ there serve different queries, and neither duplicates the other.
pub(crate) fn same_opclasses(a: &IndexStat, b: &IndexStat, n: usize) -> bool {
    let class = |i: &IndexStat, k: usize| {
        i.opclasses.as_ref().and_then(|v| v.get(k)).map(|c| c.trim().to_ascii_lowercase()).unwrap_or_default()
    };
    (0..n).all(|k| class(a, k) == class(b, k))
}

/// Whether the index is a partition's copy of a partitioned index (PostgreSQL). Every partition gets one, and a cold
/// partition reads as unused because queries prune it, not because the index is useless; such copies can't be dropped
/// one by one either. They are judged at their parent, never alone.
pub(crate) fn is_partition_copy(i: &IndexStat) -> bool {
    i.partition_of.is_some()
}

/// `b` makes `a` redundant when a's keys are a leading prefix of b's keys and everything a includes is available in b.
fn makes_redundant(b: &IndexStat, a: &IndexStat) -> bool {
    if b.columns.len() < a.columns.len() {
        return false;
    }
    let prefix = a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y))
        && same_opclasses(a, b, a.columns.len());
    let available: Vec<String> = b.key_names().iter().map(|s| s.to_string()).chain(b.include.iter().cloned()).collect();
    let includes_ok = a.include.iter().all(|c| available.iter().any(|x| x.eq_ignore_ascii_case(c)));
    prefix && includes_ok
}

/// Between two identical indexes, drop the one that is less used, then the one with the later name, so exactly one finding is produced.
fn prefer_drop(a: &IndexStat, b: &IndexStat) -> bool {
    if b.is_unique || b.is_primary_key {
        return true;
    }
    let (ra, rb) = (a.reads.unwrap_or(0.0), b.reads.unwrap_or(0.0));
    if ra != rb {
        return ra < rb;
    }
    a.name > b.name
}

pub fn unindexed_fk_info() -> CheckInfo {
    CheckInfo {
        id: "unindexed-fk".into(),
        title: "Foreign keys without an index".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Foreign key columns with no index starting with them, which makes deletes on the parent and joins from the child slow.".into(),
    }
}

/// Whether an index can serve the foreign key: it leads with the key columns, isn't disabled, and covers every row the
/// check can match. A filtered (partial) index can't, unless its filter only leaves out rows whose key is NULL (those
/// never match a parent row): checking a parent delete has to find soft-deleted children too.
pub(crate) fn fk_is_indexed(snapshot: &Snapshot, fk: &culprit_core::ForeignKey) -> bool {
    snapshot.indexes_on(&fk.schema, &fk.table).any(|i| {
        !i.is_disabled
            && leads_with(i, &fk.columns)
            && i.filter.as_deref().is_none_or(|f| only_excludes_null_keys(f, &fk.columns))
    })
}

fn leads_with(i: &IndexStat, columns: &[String]) -> bool {
    let keys = i.key_names();
    keys.len() >= columns.len()
        && lower_set(&keys[..columns.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>()) == lower_set(columns)
}

/// `([customer_id] IS NOT NULL)`, `(a IS NOT NULL AND b IS NOT NULL)`: a filter whose every condition is one of the key
/// columns being NOT NULL.
fn only_excludes_null_keys(filter: &str, columns: &[String]) -> bool {
    let plain: String = filter
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '"' | '`' | '(' | ')'))
        .collect::<String>()
        .to_ascii_lowercase();
    let words: Vec<&str> = plain.split_whitespace().collect();
    words.split(|w| *w == "and").all(|condition| match condition {
        [column, "is", "not", "null"] => {
            let name = column.rsplit('.').next().unwrap_or(column);
            columns.iter().any(|c| c.eq_ignore_ascii_case(name))
        }
        _ => false,
    })
}

/// Child tables smaller than this get no finding of their own: reading one in full is cheap today.
pub(crate) const FK_OWN_FINDING_ROWS: f64 = 10_000.0;

/// An index led by the key's columns that its filter keeps from serving the key.
fn filtered_fk_index<'a>(snapshot: &'a Snapshot, fk: &'a culprit_core::ForeignKey) -> Option<&'a IndexStat> {
    snapshot.indexes_on(&fk.schema, &fk.table).find(|i| {
        leads_with(i, &fk.columns) && i.filter.as_deref().is_some_and(|f| !only_excludes_null_keys(f, &fk.columns))
    })
}

/// Whether the database checks the key when a parent row is deleted or re-keyed. SQL Server's NOCHECK CONSTRAINT
/// switches a foreign key off entirely (`sys.foreign_keys.is_disabled`, read with the untrusted constraints): nothing
/// looks for child rows then, so nothing needs an index for it. A key that is only untrusted (added WITH NOCHECK) is
/// still checked on every delete.
pub(crate) fn fk_is_enforced(snapshot: &Snapshot, fk: &culprit_core::ForeignKey) -> bool {
    !snapshot.constraints.iter().any(|c| {
        c.kind == culprit_core::ConstraintKind::ForeignKey
            && c.disabled
            && c.name.eq_ignore_ascii_case(&fk.name)
            && c.schema.eq_ignore_ascii_case(&fk.schema)
            && c.table.eq_ignore_ascii_case(&fk.table)
    })
}

/// The foreign keys the database checks (see [`fk_is_enforced`]) that have columns.
pub(crate) fn enforced_foreign_keys(snapshot: &Snapshot) -> impl Iterator<Item = &culprit_core::ForeignKey> {
    snapshot.foreign_keys.iter().filter(|fk| !fk.columns.is_empty() && fk_is_enforced(snapshot, fk))
}

/// Every foreign key no index can serve, with its child table's rows when known, largest first. A key repeated on the
/// same columns (a duplicate constraint), or whose columns lead another unindexed key on the same table (one index on
/// the longer key serves both), is listed once. A key the database doesn't check isn't listed.
pub(crate) fn unindexed_foreign_keys(snapshot: &Snapshot) -> Vec<(&culprit_core::ForeignKey, Option<f64>)> {
    let keys = |fk: &culprit_core::ForeignKey| fk.columns.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
    let same_table = |a: &culprit_core::ForeignKey, b: &culprit_core::ForeignKey| {
        a.schema.eq_ignore_ascii_case(&b.schema) && a.table.eq_ignore_ascii_case(&b.table)
    };
    // A foreign key on a partition is the partitioned table's key, copied: judged once, at the parent.
    let is_partition =
        |fk: &culprit_core::ForeignKey| snapshot.table(&fk.schema, &fk.table).is_some_and(|t| t.partition_of.is_some());
    let candidates: Vec<&culprit_core::ForeignKey> =
        enforced_foreign_keys(snapshot).filter(|fk| !fk_is_indexed(snapshot, fk) && !is_partition(fk)).collect();
    let mut out: Vec<(&culprit_core::ForeignKey, Option<f64>)> = Vec::new();
    for fk in &candidates {
        let (mine, longer) = (keys(fk), |o: &&culprit_core::ForeignKey| {
            let theirs = keys(o);
            same_table(fk, o) && theirs.len() > fk.columns.len() && theirs.starts_with(&keys(fk))
        });
        if candidates.iter().any(longer) || out.iter().any(|(o, _)| same_table(fk, o) && keys(o) == mine) {
            continue;
        }
        out.push((fk, snapshot.table(&fk.schema, &fk.table).and_then(|t| t.rows)));
    }
    out.sort_by(|a, b| {
        b.1.unwrap_or(-1.0)
            .total_cmp(&a.1.unwrap_or(-1.0))
            .then_with(|| (&a.0.schema, &a.0.table, &a.0.name).cmp(&(&b.0.schema, &b.0.table, &b.0.name)))
    });
    out
}

pub fn unindexed_fk(snapshot: &Snapshot) -> Vec<Finding> {
    let unindexed = unindexed_foreign_keys(snapshot);
    let mut out: Vec<Finding> = unindexed
        .iter()
        .filter_map(|(fk, rows)| {
            rows.filter(|r| *r >= FK_OWN_FINDING_ROWS).map(|r| unindexed_fk_finding(snapshot, fk, r))
        })
        .collect();
    out.extend(unindexed_fk_pattern(snapshot, &unindexed));
    out
}

fn unindexed_fk_finding(snapshot: &Snapshot, fk: &culprit_core::ForeignKey, rows: f64) -> Finding {
    let child = format!("{}.{}", fk.schema, fk.table);
    let parent = format!("{}.{}", fk.ref_schema, fk.ref_table);
    let cols = format::list(&fk.columns);
    let mut f = finding(
        "unindexed-fk",
        &format!("{child}({})", fk.columns.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &child),
    );
    f.title = format!("{} has no index on {}, its foreign key to {}", fk.table, cols, fk.ref_table);
    f.summary = format!(
        "Deleting a row from {parent}, or changing its key, makes the database look for matching rows in {child} ({} rows) without an index, which means reading the whole table. Queries that load {} rows for one {} row can't seek either.",
        format::count(rows),
        fk.table,
        fk.ref_table
    );
    // Queries that go through the key, still running: a plain insert into the child only validates the key against the
    // parent's primary key, which needs no index here.
    let touching: Vec<&QueryStat> = queries_touching(snapshot, &fk.schema, &fk.table)
        .into_iter()
        .filter(|q| crate::advisor::uses_foreign_key(&q.text, &fk.columns, &fk.ref_table))
        .filter(|q| !crate::query_is_history(snapshot, q))
        .collect();
    let quiet_parent = parent_never_deletes(snapshot, fk);
    f.severity = if rows >= 1_000_000.0 && quiet_parent.is_none() { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + (rows.log10() - 4.0).max(0.0) * 2.0;
    f.impact.label = format!("{} rows unindexed", format::count(rows));
    f.confidence = if quiet_parent.is_some() { Confidence::Low } else { Confidence::High };
    if let Some(note) = &quiet_parent {
        f.summary.push(' ');
        f.summary.push_str(note);
    }
    if let Some(note) = lookup_key_caveat(snapshot, fk, rows) {
        f.caveats.push(note);
    }
    f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
    f.metrics.push(Metric::text("Foreign key", fk.name.clone()));
    f.queries = query_refs(&touching.into_iter().take(5).collect::<Vec<_>>());
    f.fix = Some(Fix {
        summary: format!("Index the foreign key column{} on {}.", if fk.columns.len() > 1 { "s" } else { "" }, child),
        sql: Some(create_index_sql(snapshot, &fk.schema, &fk.table, &fk.columns, &[])),
        cost: write_cost(snapshot, &fk.schema, &fk.table),
    });
    f.verify =
        vec![format!("Deletes from {parent} should no longer scan {child}; re-scan to confirm the finding is gone.")];
    if let Some(i) = filtered_fk_index(snapshot, fk) {
        f.caveats.push(filtered_caveat(i, &parent));
    }
    f
}

/// The sentence that says a key's parent table has deleted nothing, when PostgreSQL or MySQL counted its deletes for
/// at least a week ([`watched_long_enough`]): the check that needs the index runs only when a parent row is deleted or
/// its key changes, so then the index is for joins, not for the check. `None` when it deleted rows, or nobody knows.
pub(crate) fn parent_never_deletes(snapshot: &Snapshot, fk: &culprit_core::ForeignKey) -> Option<String> {
    let parent = snapshot.table(&fk.ref_schema, &fk.ref_table)?;
    if parent.deletes != Some(0.0) || !watched_long_enough(snapshot) {
        return None;
    }
    Some(format!(
        "{}.{} has had no deletes {}{}, though: the database looks for matching rows only when a parent row is deleted \
         or its key changes, so the index would mostly serve joins.",
        fk.ref_schema,
        fk.ref_table,
        since_phrase(snapshot),
        if parent.updates == Some(0.0) { " and no updates" } else { "" }
    ))
}

/// A parent table so small (a lookup table) that each of its keys matches a large share of the child's rows: an index
/// on the key makes a parent delete cheap, but a read of one key's rows would still scan.
fn lookup_key_caveat(snapshot: &Snapshot, fk: &culprit_core::ForeignKey, rows: f64) -> Option<String> {
    let parent_rows = snapshot.table(&fk.ref_schema, &fk.ref_table)?.rows.filter(|r| *r >= 1.0)?;
    if parent_rows > 1.0 / culprit_core::INDEX_KEEPS_AT_MOST {
        return None;
    }
    Some(format!(
        "{}.{} has only {} rows, so each of its keys matches about {} rows of {}.{}: the index makes deleting a parent row \
         cheap, but a query that reads one key's rows would still read most of the table.",
        fk.ref_schema,
        fk.ref_table,
        format::count(parent_rows),
        format::count(rows / parent_rows),
        fk.schema,
        fk.table
    ))
}

fn filtered_caveat(i: &IndexStat, parent: &str) -> String {
    format!(
        "{} leads with these columns but is filtered ({}), so it can't serve the key: a delete on {parent} has to find \
         the rows the filter leaves out too.",
        i.name,
        i.filter.as_deref().unwrap_or_default()
    )
}

/// One finding for the habit rather than the table: many foreign keys unindexed, mostly on tables too small for a
/// finding of their own. A schema helper or ORM that declares keys without indexes is the usual cause, and fixing it
/// there covers the keys still to come.
fn unindexed_fk_pattern(
    snapshot: &Snapshot,
    unindexed: &[(&culprit_core::ForeignKey, Option<f64>)],
) -> Option<Finding> {
    let total = enforced_foreign_keys(snapshot).count();
    let small: Vec<(&culprit_core::ForeignKey, f64)> =
        unindexed.iter().filter_map(|(fk, rows)| rows.filter(|r| *r < FK_OWN_FINDING_ROWS).map(|r| (*fk, r))).collect();
    if small.len() < 3 || (unindexed.len() as f64) < total as f64 * 0.25 {
        return None;
    }
    let big = unindexed.iter().filter(|(_, rows)| rows.is_some_and(|r| r >= FK_OWN_FINDING_ROWS)).count();
    let unknown = unindexed.iter().filter(|(_, rows)| rows.is_none()).count();
    let filtered: Vec<&IndexStat> = unindexed.iter().filter_map(|(fk, _)| filtered_fk_index(snapshot, fk)).collect();
    let named = |fk: &culprit_core::ForeignKey| format!("{}({})", fk.table, fk.columns.join(", "));
    let under = format::count(FK_OWN_FINDING_ROWS);
    let mut f =
        finding("unindexed-fk", "schema", Category::Indexes, subject(SubjectKind::Database, &snapshot.server.database));
    f.title = if unindexed.len() == total {
        format!("None of the {total} foreign keys has an index that can serve it")
    } else {
        format!("{} of {} foreign keys have no index that can serve them", unindexed.len(), total)
    };
    let names = small
        .iter()
        .take(4)
        .map(|(fk, rows)| format!("{} ({} rows)", named(fk), format::count(*rows)))
        .collect::<Vec<_>>();
    let elsewhere = [
        match big {
            0 => None,
            1 => Some("1 on a larger table has a finding of its own".to_string()),
            n => Some(format!("{n} on larger tables have findings of their own")),
        },
        match unknown {
            0 => None,
            1 => Some("1 is on a table whose size isn't known (a partitioned table, for one)".to_string()),
            n => Some(format!("{n} are on tables whose size isn't known (partitioned tables, for one)")),
        },
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    f.summary = format!(
        "{}{}; {} {}. Deleting a parent row, or changing its key, checks every child table for matching rows, and \
         without an index that check reads the child table in full: cheap while the tables are small, slower with every \
         row and with every new table declared the same way.{} When this many keys lack an index, the code that declares \
         tables usually creates foreign keys without one: fixing it there covers the keys still to come.",
        if elsewhere.is_empty() {
            format!("All {} are on tables under {under} rows", small.len())
        } else {
            format!("{} of these keys are on tables under {under} rows", small.len())
        },
        if elsewhere.is_empty() { String::new() } else { format!(" ({})", format::list(&elsewhere)) },
        if small.len() <= 4 { "they are" } else { "the largest are" },
        format::list(&names),
        match filtered.len() {
            0 => String::new(),
            1 => format!(
                " {} is led by its key's columns but filtered, so it can't serve the check: a parent delete has to find the \
                 rows the filter leaves out too.",
                filtered[0].name
            ),
            n => format!(
                " {n} of these keys are covered only by a filtered index, which can't serve the check: a parent delete has \
                 to find the rows the filter leaves out too."
            ),
        }
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + (small.len() as f64).log2().min(4.0);
    f.impact.label = format!("{} of {} foreign keys unindexed", unindexed.len(), total);
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Foreign keys without a usable index", unindexed.len() as f64, Unit::Count));
    f.metrics.push(Metric::new("Foreign keys", total as f64, Unit::Count));
    if !filtered.is_empty() {
        f.metrics.push(Metric::new("Covered only by a filtered index", filtered.len() as f64, Unit::Count));
    }
    f.fix = Some(Fix {
        summary: format!(
            "Give each foreign key an unfiltered index led by its columns, and make declaring a foreign key declare its \
             index wherever the schema is defined (a migration helper, the ORM's table definitions), so new keys get one \
             too.{}",
            if snapshot.server.engine == Engine::Postgres {
                " Run the statements one at a time (psql -f does): CREATE INDEX CONCURRENTLY can't run inside a transaction."
            } else {
                ""
            }
        ),
        sql: Some(
            small
                .iter()
                .map(|(fk, _)| create_index_sql(snapshot, &fk.schema, &fk.table, &fk.columns, &[]))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        cost: Some(
            "Each index is on a small table and costs little to keep up; check the busiest tables' write rates first."
                .into(),
        ),
    });
    f.verify = vec![
        "Re-scan: this finding is gone; the foreign-key findings for single tables go once their own fixes are applied."
            .into(),
    ];
    f.caveats.push(
        "A key whose parent rows are never deleted or re-keyed (a lookup table, append-only history) doesn't need the \
         index for the check; leave those out if their write cost matters."
            .into(),
    );
    Some(f)
}

pub fn filtered_scan_info() -> CheckInfo {
    CheckInfo {
        id: "filtered-scan".into(),
        title: "Scans an index would avoid".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::QueryPlans],
        description: "Heavy queries whose plan reads a large table in full and keeps only the rows matching a filter that no index supports.".into(),
    }
}

pub(crate) struct ScanGroup<'a> {
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) keys: Vec<String>,
    pub(crate) predicate: String,
    pub(crate) rows: f64,
    pub(crate) queries: Vec<(&'a QueryStat, u32)>,
}

pub fn filtered_scan(snapshot: &Snapshot) -> Vec<Finding> {
    scan_groups(snapshot).into_iter().map(|g| filtered_scan_finding(snapshot, g)).collect()
}

/// Full scans of large tables that keep only rows matching a filter no index leads with, grouped by table and keys.
pub(crate) fn scan_groups(snapshot: &Snapshot) -> Vec<ScanGroup<'_>> {
    let mut groups: Vec<ScanGroup> = Vec::new();
    for q in app_queries(snapshot) {
        let Some(plan) = &q.plan else { continue };
        if query_impact(snapshot, &[q]).is_none_or(|i| i.share < 0.005) {
            continue;
        }
        for node in plan.nodes() {
            if node.kind != culprit_core::NodeKind::Scan {
                continue;
            }
            let (Some(object), Some(predicate)) = (&node.object, &node.predicate) else { continue };
            let Some((schema, table)) = split_object(object) else { continue };
            let rows = scan_rows(snapshot, node, &schema, &table).unwrap_or(0.0);
            let estimate_keeps_few =
                node.est_rows_total() <= rows * node.est_executions.max(1.0) * culprit_core::INDEX_KEEPS_AT_MOST;
            if rows < 10_000.0 || !(estimate_keeps_few || measured_keeps_few(snapshot, q, rows)) {
                continue;
            }
            let cols = predicate_columns(predicate);
            let mut keys = cols.equality.clone();
            keys.extend(cols.range.into_iter().take(1));
            if keys.is_empty() || leading_index_exists(snapshot, &schema, &table, &cols.equality, &keys) {
                continue;
            }
            match groups
                .iter_mut()
                .find(|g| g.schema == schema && g.table == table && lower_set(&g.keys) == lower_set(&keys))
            {
                Some(g) => {
                    if !g.queries.iter().any(|(x, _)| x.id == q.id) {
                        g.queries.push((q, node.id));
                    }
                }
                None => groups.push(ScanGroup {
                    schema,
                    table,
                    keys,
                    predicate: predicate.clone(),
                    rows,
                    queries: vec![(q, node.id)],
                }),
            }
        }
    }
    groups
}

/// Rows a plan's scan reads. A scan of a filtered index reads only that index's rows, which the plan's estimate of
/// rows read gives; its table cardinality is the whole table's, and would make a scan of an empty filtered index (a
/// queue's "pending" rows, say) look like a read of every row. Any other scan is taken at the table's size: under a
/// TOP, the estimate of rows read assumes matches come early, which is exactly when a costly scan would hide.
pub(crate) fn scan_rows(snapshot: &Snapshot, node: &culprit_core::PlanNode, schema: &str, table: &str) -> Option<f64> {
    let filtered = node.index.as_deref().is_some_and(|index| {
        let name = index.trim_matches(['[', ']', '"']);
        snapshot.indexes.iter().any(|i| {
            i.schema.eq_ignore_ascii_case(schema)
                && i.table.eq_ignore_ascii_case(table)
                && i.name.eq_ignore_ascii_case(name)
                && i.filter.is_some()
        })
    });
    if filtered {
        return node.rows_read;
    }
    node.table_rows.or_else(|| snapshot.table(schema, table).and_then(|t| t.rows))
}

/// MySQL and MariaDB count rows examined and rows returned per statement. A statement that examines most of the
/// table on every run and returns under a tenth of it filters after reading, whatever the optimizer estimated (MariaDB
/// assumes a condition keeps every row when the column has no statistics). Rows returned measure what the filter kept
/// only when the statement returns every row it keeps (see [`returns_every_kept_row`]): `SELECT COUNT(*) … WHERE
/// created_at > ?` returns one row however many the filter keeps, so there the plan's own estimate decides.
fn measured_keeps_few(snapshot: &Snapshot, q: &QueryStat, table_rows: f64) -> bool {
    if snapshot.server.engine != Engine::Mysql || q.executions <= 0.0 || !returns_every_kept_row(&q.text) {
        return false;
    }
    let examined = q.logical_reads.map(|r| r / q.executions).unwrap_or(0.0);
    let returned = q.rows.map(|r| r / q.executions);
    examined >= table_rows * 0.5 && returned.is_some_and(|r| r <= table_rows * culprit_core::INDEX_KEEPS_AT_MOST)
}

/// Whether a statement returns every row its filter keeps, so that rows returned measure the filter: a SELECT from one
/// table, without aggregates, GROUP BY, DISTINCT, LIMIT (or TOP, FETCH), HAVING, a set operation or a subquery. Any of
/// those returns fewer rows than the filter keeps, and a join can return fewer or more.
fn returns_every_kept_row(text: &str) -> bool {
    const AGGREGATES: [&str; 14] = [
        "COUNT",
        "SUM",
        "AVG",
        "MIN",
        "MAX",
        "GROUP_CONCAT",
        "JSON_ARRAYAGG",
        "JSON_OBJECTAGG",
        "STD",
        "STDDEV",
        "STDDEV_POP",
        "STDDEV_SAMP",
        "VARIANCE",
        "BIT_OR",
    ];
    let plain = text.replace(['`', '"', '[', ']'], "").to_ascii_uppercase();
    let words: Vec<&str> =
        plain.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
    let calls = |name: &str| {
        plain.match_indices(name).any(|(at, _)| {
            let before = plain[..at].chars().next_back();
            let after = plain[at + name.len()..].trim_start();
            !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') && after.starts_with('(')
        })
    };
    words.first() == Some(&"SELECT")
        && words.iter().filter(|w| **w == "SELECT").count() == 1
        && !words.iter().any(|w| {
            matches!(
                *w,
                "GROUP" | "DISTINCT" | "LIMIT" | "TOP" | "FETCH" | "HAVING" | "UNION" | "INTERSECT" | "EXCEPT" | "JOIN"
            )
        })
        && !AGGREGATES.iter().any(|a| calls(a))
        && culprit_core::sql::tables_in(text, Engine::Mysql).len() == 1
}

pub(crate) fn split_object(object: &str) -> Option<(String, String)> {
    let clean = object.replace(['[', ']', '"'], "");
    let parts: Vec<&str> = clean.split('.').collect();
    match parts.as_slice() {
        [.., schema, table] => Some((schema.to_string(), table.to_string())),
        [table] => Some(("dbo".to_string(), table.to_string())),
        _ => None,
    }
}

/// A disabled index that would lead with the scan's columns ([`leading_index`]'s rule): the fix is to bring it back,
/// not to build its twin.
pub(crate) fn disabled_leading_index<'a>(snapshot: &'a Snapshot, g: &ScanGroup) -> Option<&'a IndexStat> {
    let cols = predicate_columns(&g.predicate);
    let wanted: &[String] = if cols.equality.is_empty() { g.keys.get(..1)? } else { &cols.equality };
    snapshot.indexes.iter().find(|i| {
        let existing = i.key_names();
        i.schema.eq_ignore_ascii_case(&g.schema)
            && i.table.eq_ignore_ascii_case(&g.table)
            && i.is_disabled
            && existing.len() >= wanted.len()
            && lower_set(&existing[..wanted.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                == lower_set(wanted)
    })
}

/// An index already leads with the equality columns: the scan has another cause (type mismatch, function on the column, stale stats).
fn leading_index_exists(snapshot: &Snapshot, schema: &str, table: &str, equality: &[String], keys: &[String]) -> bool {
    leading_index(snapshot, schema, table, equality, keys).is_some()
}

/// The index that leads with the equality columns (or, without any, with the first key), in any order.
pub(crate) fn leading_index<'a>(
    snapshot: &'a Snapshot,
    schema: &str,
    table: &str,
    equality: &[String],
    keys: &[String],
) -> Option<&'a IndexStat> {
    let wanted = if equality.is_empty() { keys.get(..1)? } else { equality };
    snapshot.indexes.iter().find(|i| {
        let existing = i.key_names();
        i.schema.eq_ignore_ascii_case(schema)
            && i.table.eq_ignore_ascii_case(table)
            && !i.is_disabled
            && existing.len() >= wanted.len()
            && lower_set(&existing[..wanted.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                == lower_set(wanted)
    })
}

fn filtered_scan_finding(snapshot: &Snapshot, g: ScanGroup) -> Finding {
    let table_name = format!("{}.{}", g.schema, g.table);
    let key_text = format::list(&g.keys);
    let queries: Vec<&QueryStat> = g.queries.iter().map(|(q, _)| *q).collect();
    let impact = query_impact(snapshot, &queries).expect("queries have workload");
    let mut f = finding(
        "filtered-scan",
        &format!("{table_name}({})", g.keys.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    apply_query_impact(&mut f, &impact);
    let time_ms: f64 = queries.iter().map(|q| q.total_time_ms).sum();
    f.severity = severity_for_time(snapshot, time_ms, impact.share);
    f.confidence = Confidence::High;
    f.title = format!("{} is read in full to find rows by {}", g.table, key_text);
    let disabled = disabled_leading_index(snapshot, &g);
    let fk = snapshot.foreign_keys.iter().find(|fk| {
        fk.schema.eq_ignore_ascii_case(&g.schema)
            && fk.table.eq_ignore_ascii_case(&g.table)
            && !fk.columns.is_empty()
            && fk.columns.iter().all(|c| g.keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
    });
    f.summary = format!(
        "{} read every row of {} ({} rows) and keep only those matching `{}`. {}, so the database has no faster way in. Together they account for {} of database time.{}",
        if queries.len() == 1 {
            "One query plans to".to_string()
        } else {
            format!("{} queries plan to", queries.len())
        },
        table_name,
        format::count(g.rows),
        g.predicate.trim(),
        match disabled {
            Some(i) => format!("Only {}, which is disabled, starts with {}", i.name, key_text),
            None => format!("No index starts with {key_text}"),
        },
        format::percent(impact.share),
        fk.map(|fk| format!(
            " {} is also the foreign key to {}, so deletes there scan this table too.",
            key_text, fk.ref_table
        ))
        .unwrap_or_default()
    );
    f.queries = g
        .queries
        .iter()
        .map(|(q, node)| culprit_core::QueryRef { id: q.id.clone(), plan_node: Some(*node), note: None })
        .collect();
    f.metrics.push(Metric::new("Share of DB time", impact.share, Unit::Ratio));
    if let Some(per_day_ms) = impact.per_day_ms {
        f.metrics.push(Metric::new("DB time per day", per_day_ms, Unit::Ms));
    }
    f.metrics.push(Metric::new("Rows scanned per run", g.rows, Unit::Rows));
    f.metrics.push(Metric::new("Queries", queries.len() as f64, Unit::Count));
    f.fix = Some(match disabled {
        Some(i) => widen_fix(snapshot, i, &[]),
        None => Fix {
            summary: format!("Create an index on {} so these queries seek instead of scanning.", key_text),
            sql: Some(create_index_sql(snapshot, &g.schema, &g.table, &g.keys, &[])),
            cost: write_cost(snapshot, &g.schema, &g.table),
        },
    });
    f.verify = vec![
        format!(
            "Explain one of the queries again: the scan of {} should become an index seek/scan on {}.",
            g.table,
            disabled.map_or("the new index".to_string(), |i| i.name.clone())
        ),
        "Re-scan after the workload has run: this finding should be gone and the queries' share of database time lower.".into(),
    ];
    f
}

/// Indexes another index makes redundant, mapped to the index that covers them. Shared by the duplicate and unused checks so an index is reported once.
///
/// The keeper is never a clustered index (InnoDB's PRIMARY included, which MySQL reports with the same kind as any
/// other): seeking it reads whole rows, so a narrow index on its leading columns serves reads it can't serve as cheaply.
pub fn redundant_indexes(snapshot: &Snapshot) -> Vec<(&IndexStat, &IndexStat)> {
    let mut out = Vec::new();
    for a in &snapshot.indexes {
        if a.is_primary_key
            || a.is_unique
            || a.is_clustered
            || a.is_disabled
            || a.columns.is_empty()
            || is_partition_copy(a)
        {
            continue;
        }
        let keeper = snapshot
            .indexes_on(&a.schema, &a.table)
            .filter(|b| {
                b.name != a.name && !b.is_disabled && !b.is_clustered && b.filter == a.filter && b.kind == a.kind
            })
            .find(|b| makes_redundant(b, a) && (!same_keys(a, b) || prefer_drop(a, b)))
            // A read index isn't redundant to one nothing reads: drop the unread one instead (the unused check says so).
            .filter(|b| !(is_unused(snapshot, b) && a.reads.is_some_and(|r| r > 0.0)));
        if let Some(b) = keeper {
            out.push((a, b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_made_of_literal_equalities() {
        let pairs =
            |f: &str| filter_equalities(f).map(|v| v.into_iter().map(|(c, l)| format!("{c}={l}")).collect::<Vec<_>>());
        assert_eq!(pairs("([is_deleted]=(0))"), Some(vec!["is_deleted=0".to_string()]));
        assert_eq!(
            pairs("([status]='open' AND [kind]=(2))"),
            Some(vec!["status='open'".to_string(), "kind=2".to_string()])
        );
        assert_eq!(pairs("([customer_id] IS NOT NULL)"), None);
        assert_eq!(pairs("([is_deleted]=(0) AND [due_at]>=getdate())"), None);
        assert_eq!(pairs("([status]<>'closed')"), None);
    }

    #[test]
    fn a_parameter_compared_on_either_side() {
        assert!(compares_to_parameter(
            "select id from [t] where (([a] = @p0) and ([is_deleted] = @par1))",
            "is_deleted"
        ));
        assert!(compares_to_parameter("SELECT id FROM t WHERE @deleted = t.is_deleted", "is_deleted"));
        assert!(!compares_to_parameter("SELECT id FROM t WHERE is_deleted = 0 AND a = @p", "is_deleted"));
        assert!(!compares_to_parameter("SELECT id FROM t WHERE is_deleted >= @p", "is_deleted"));
        assert!(!compares_to_parameter("SELECT id FROM t WHERE was_is_deleted = @p", "is_deleted"));
        assert!(!compares_to_parameter("SELECT id FROM t WHERE @p <= is_deleted", "is_deleted"));
    }

    #[test]
    fn a_literal_compared_on_either_side() {
        let fits = |text: &str, literal: &str| compared_to(text, "status", |v| same_literal(v, literal));
        assert!(fits("SELECT id FROM t WHERE [t].[status] = N'open' AND a = @p", "'open'"));
        assert!(fits("SELECT id FROM t WHERE 'open' = status", "N'open'"));
        assert!(fits("SELECT id FROM t WHERE (status = (2))", "2"));
        assert!(fits("SELECT id FROM t WHERE status=2.0", "(2)"));
        assert!(!fits("SELECT id FROM t WHERE status = 'closed'", "'open'"));
        assert!(!fits("SELECT id FROM t WHERE status = @p", "'open'"));
        assert!(!fits("SELECT id FROM t WHERE status >= 2", "2"));
        assert!(!fits("SELECT id FROM t WHERE 2 <= status", "2"));
        assert!(!fits("SELECT id FROM t WHERE a = 2", "2"));
    }
}
