//! Filtered-index parameter mismatch detection (`ParameterizedFilter`): when an existing filtered index would serve an
//! optimizer request if its queries wrote the filter's literal instead of passing a parameter.

use super::candidates::index_filter_equalities;
use super::*;

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
        let conditions = index_filter_equalities(i)?;
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

/// Whether `text` compares `table`'s `column` to a parameter: `[column] = @p`, `t.column = @p` or `@p = column`.
pub(in crate::checks::indexes) fn compares_to_parameter(text: &str, table: &str, column: &str) -> bool {
    compared_to(text, table, column, |value| value.starts_with('@') && value.len() > 1)
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
