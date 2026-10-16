//! The `missing-index` check: Index suggestions recorded by SQL Server's optimizer, merged with each other and checked against existing indexes.

use super::*;

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

check!(MissingIndexCheck: missing_index_info, missing_index);

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
pub(super) fn filtered_bystander<'a>(
    snapshot: &'a Snapshot,
    c: &Candidate,
    asking: &[&QueryStat],
) -> Option<&'a IndexStat> {
    same_keys_on_table(snapshot, c).find(|i| i.filter.as_deref().is_some_and(|f| !fits_filter(f, asking)))
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

/// Every query in `asking` (at least one) compares each of the filter's columns to the filter's own literal.
pub(super) fn fits_filter(filter: &str, asking: &[&QueryStat]) -> bool {
    let Some(conditions) = filter_equalities(filter) else { return false };
    !asking.is_empty()
        && asking.iter().all(|q| {
            conditions
                .iter()
                .all(|(column, literal)| compared_to(&q.text, column, |value| same_literal(value, literal)))
        })
}

/// A filter as people read it: "is_deleted = 0", or the definition without its brackets.
pub(super) fn readable_filter(filter: &str) -> String {
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
            f.severity = unlinked_severity(snapshot, &c, lookups);
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
        None => {
            f.severity = unlinked_severity(snapshot, &c, lookups);
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
pub(super) fn wanted_before_window(snapshot: &Snapshot, c: &Candidate) -> Option<(String, String)> {
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

pub(super) fn is_literal(word: &str) -> bool {
    word.parse::<f64>().is_ok() || (word.ends_with('\'') && (word.starts_with('\'') || word.starts_with("N'")))
}

/// Whether `text` compares `column` to a parameter: `[column] = @p`, `t.column = @p` or `@p = column`.
pub(super) fn compares_to_parameter(text: &str, column: &str) -> bool {
    compared_to(text, column, |value| value.starts_with('@') && value.len() > 1)
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

/// Whether `text` compares `column` with `=` to a value `wanted` accepts, on either side, the column perhaps bracketed
/// or qualified: `[column] = @p`, `t.column = 0`, `N'open' = column`. Text is compared lower-cased.
pub(super) fn compared_to(text: &str, column: &str, wanted: impl Fn(&str) -> bool) -> bool {
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
