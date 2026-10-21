//! Redact literal values before a plan, a comparison or a finding leaves the machine (a PR, a ticket, a chat). Strings
//! and numbers in SQL text and predicates become `'?'` and `?`; names (tables, columns, indexes) and parameter markers
//! (`@P1`, `$1`, `?`, `:name`) stay, so the text still explains the query. Each engine's quoting is followed: SQL
//! Server's `[names]` and `N'…'`, Postgres's `"names"`, `E'…'` and `$$…$$`, MySQL's `` `names` ``, `"…"` strings and
//! backslash escapes.

use crate::finding::Finding;
use crate::model::{Engine, Properties, Snapshot};
use crate::plan::{Plan, PlanNode};

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

/// Query hints whose number tunes the plan (`OPTION (MAXDOP 1)`, `FAST 10`), not data: what a plan's reading of the
/// statement depends on, kept.
const HINTS_WITH_A_NUMBER: &[&str] = &["MAXDOP", "FAST", "MAXRECURSION", "QUERYTRACEON"];

/// Numbers inside these types' parentheses are sizes (`nvarchar(200)`, `decimal(12,2)`), not data.
const SIZED_TYPES: &[&str] = &[
    "char",
    "varchar",
    "nchar",
    "nvarchar",
    "binary",
    "varbinary",
    "decimal",
    "numeric",
    "float",
    "datetime2",
    "datetimeoffset",
    "time",
    "timestamp",
    "timestamptz",
    "varying",
    "bit",
    "interval",
];

/// Replace literal values in SQL text (a statement or a plan predicate) with placeholders.
pub fn redact_sql(text: &str, engine: Engine) -> String {
    redact(text, engine, true)
}

/// Prose that quotes SQL in `code spans` (a finding's summary, caveats, notes): the values inside the spans become
/// placeholders; the words around them stay as written, apostrophes included. Checks put every piece of SQL they quote
/// in a code span, the parameter values a plan was compiled for too.
pub fn redact_prose(text: &str, engine: Engine) -> String {
    // Values first, wherever they are: a value can hold a backtick, which would end its code span early and leave the
    // values after it looking like prose.
    mask_prose_values(text, engine)
        .split('`')
        .enumerate()
        .map(|(i, part)| if i % 2 == 1 { collapse_parameters(redact_sql(part, engine)) } else { part.to_string() })
        .collect::<Vec<_>>()
        .join("`")
}

/// String literals and GUIDs anywhere in prose become placeholders. A quote right after a letter or digit is an
/// apostrophe (isn't, the plan's), unless that letter stands alone as a string prefix (`N'…'`, `E'…'`, `X'…'`,
/// `B'…'`). A literal that never closes hides the rest of the text: better a sentence lost than a value leaked.
fn mask_prose_values(text: &str, engine: Engine) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let prev = i.checked_sub(1).and_then(|k| chars.get(k));
        if chars[i] == '\'' {
            let prefix = prev.is_some_and(|p| matches!(p.to_ascii_uppercase(), 'N' | 'E' | 'X' | 'B'))
                && !word(i.checked_sub(2).and_then(|k| chars.get(k)));
            if !word(prev) || prefix {
                let backslashes = engine == Engine::Mysql || prev.is_some_and(|p| p.eq_ignore_ascii_case(&'E'));
                let mut j = i + 1;
                let close = loop {
                    match chars.get(j) {
                        None => break None,
                        Some('\\') if backslashes => j += 2,
                        Some('\'') if chars.get(j + 1) == Some(&'\'') => j += 2,
                        Some('\'') => break Some(j),
                        Some(_) => j += 1,
                    }
                };
                out.push_str("'?'");
                match close {
                    Some(end) => i = end + 1,
                    None => return out,
                }
                continue;
            }
        }
        if !word(prev) && is_guid(&chars[i..]) {
            out.push('?');
            i += 36;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `chars` starts with a GUID (8-4-4-4-12 hex digits) that isn't part of a longer word.
fn is_guid(chars: &[char]) -> bool {
    chars.len() >= 36
        && chars[..36].iter().enumerate().all(|(n, c)| match n {
            8 | 13 | 18 | 23 => *c == '-',
            _ => c.is_ascii_hexdigit(),
        })
        && !chars.get(36).is_some_and(|c| c.is_alphanumeric() || *c == '_')
}

/// A list of parameters whose values are all placeholders now (`@p0 = N'?', @p1 = N'?', …`) says nothing past its
/// first item.
fn collapse_parameters(span: String) -> String {
    let items: Vec<&str> = span.split(", ").collect();
    let placeholder = |item: &str| {
        item.split_once(" = ").is_some_and(|(name, value)| {
            name.starts_with(['@', '$', ':']) && matches!(value, "?" | "(?)" | "'?'" | "N'?'" | "E'?'")
        })
    };
    if items.len() > 1 && items.iter().all(|i| placeholder(i)) { format!("{}, …", items[0]) } else { span }
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

/// Strings only: for text where bare numbers are measurements (costs, row counts), not data.
pub fn redact_strings(text: &str, engine: Engine) -> String {
    redact(text, engine, false)
}

fn redact(text: &str, engine: Engine, numbers: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    // For each open parenthesis: whether it belongs to a sized type, so numbers in it are sizes.
    let mut parens: Vec<bool> = Vec::new();
    let mut last_word = String::new();
    let mut i = 0;
    let at = |k: usize| chars.get(k).copied();
    while i < chars.len() {
        let c = chars[i];
        let next = at(i + 1);
        match c {
            '-' if next == Some('-') && (engine != Engine::Mysql || at(i + 2).is_none_or(|n| n.is_whitespace())) => {
                let end = chars[i..].iter().position(|&x| x == '\n').map_or(chars.len(), |p| i + p);
                out.push_str(if end > i + 2 { "-- …" } else { "--" });
                i = end;
            }
            '#' if engine == Engine::Mysql => {
                let end = chars[i..].iter().position(|&x| x == '\n').map_or(chars.len(), |p| i + p);
                out.push_str("# …");
                i = end;
            }
            '/' if next == Some('*') => {
                let end = find(&chars, i + 2, &['*', '/']).map_or(chars.len(), |p| p + 2);
                let body: String = chars[i + 2..end.saturating_sub(2).max(i + 2)].iter().collect();
                // Optimizer hints and Culprit's own tag carry no data; any other comment might.
                if body.trim_start().starts_with('+') || body.trim() == "culprit" {
                    out.extend(&chars[i..end]);
                } else {
                    out.push_str("/* … */");
                }
                i = end;
            }
            '\'' => {
                let escapes = engine == Engine::Mysql || last_word.eq_ignore_ascii_case("e");
                let end = skip_quoted(&chars, i, '\'', escapes);
                // A pattern keeps where its wildcards are (`'%?%'`): that's its shape, not data, and a leading % is
                // what stops an index from being used.
                let body = &chars[(i + 1).min(end)..end.saturating_sub(1).max(i + 1).min(chars.len())];
                out.push('\'');
                if body.first() == Some(&'%') {
                    out.push('%');
                }
                out.push('?');
                if body.len() > 1 && body.last() == Some(&'%') {
                    out.push('%');
                }
                out.push('\'');
                i = end;
                last_word.clear();
            }
            '"' if engine == Engine::Mysql => {
                i = skip_quoted(&chars, i, '"', true);
                out.push_str("\"?\"");
                last_word.clear();
            }
            '"' => {
                let end = skip_quoted(&chars, i, '"', false);
                out.extend(&chars[i..end]);
                last_word = chars[i + 1..end.saturating_sub(1).max(i + 1)].iter().collect();
                i = end;
            }
            '[' if engine == Engine::Mssql => {
                let end = skip_quoted(&chars, i, ']', false);
                out.extend(&chars[i..end]);
                last_word = chars[i + 1..end.saturating_sub(1).max(i + 1)].iter().collect();
                i = end;
            }
            '`' if engine == Engine::Mysql => {
                let end = skip_quoted(&chars, i, '`', false);
                out.extend(&chars[i..end]);
                last_word.clear();
                i = end;
            }
            '$' if engine == Engine::Postgres => {
                if let Some(tag_end) = dollar_tag(&chars, i) {
                    let tag: Vec<char> = chars[i..=tag_end].to_vec();
                    let close = find(&chars, tag_end + 1, &tag).map_or(chars.len(), |p| p + tag.len());
                    out.push_str("'?'");
                    i = close;
                } else {
                    // `$1`: a parameter marker, kept.
                    let end = word_end(&chars, i + 1);
                    out.extend(&chars[i..end]);
                    i = end;
                }
                last_word.clear();
            }
            '@' | ':' if next.is_some_and(|n| n.is_alphanumeric() || n == '_' || n == '@') => {
                // `@P1`, `@@ROWCOUNT`, `:name`: markers and variables, not data. (`::` casts fall through below.)
                if c == ':' && next == Some(':') {
                    out.push_str("::");
                    i += 2;
                    continue;
                }
                let end = word_end(&chars, i + 1 + usize::from(next == Some('@')));
                out.extend(&chars[i..end]);
                last_word.clear();
                i = end;
            }
            '(' => {
                parens.push(SIZED_TYPES.iter().any(|t| t.eq_ignore_ascii_case(&last_word)));
                out.push(c);
                last_word.clear();
                i += 1;
            }
            ')' => {
                parens.pop();
                out.push(c);
                i += 1;
            }
            _ if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) => {
                let end = number_end(&chars, i);
                let hint = HINTS_WITH_A_NUMBER.iter().any(|h| h.eq_ignore_ascii_case(&last_word));
                if numbers && !parens.last().copied().unwrap_or(false) && !hint {
                    out.push('?');
                } else {
                    out.extend(&chars[i..end]);
                }
                i = end;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let end = word_end(&chars, i);
                last_word = chars[i..end].iter().collect();
                out.push_str(&last_word);
                i = end;
            }
            _ => {
                if !c.is_whitespace() && c != ',' {
                    last_word.clear();
                }
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// SQL Server names columns in plans as `[database].[schema].[table].[column]`. Drop the database part, so an export
/// doesn't name the database and plans from two environments read the same.
pub fn strip_database(text: &str) -> String {
    strip_parts(text, 3)
}

fn strip_parts(text: &str, keep: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '[' || (i > 0 && (chars[i - 1] == '.' || chars[i - 1] == ']')) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Up to four bracketed parts joined by dots.
        let mut parts = Vec::new();
        let mut j = i;
        while j < chars.len() && chars[j] == '[' && parts.len() < 4 {
            let end = skip_quoted(&chars, j, ']', false);
            parts.push((j, end));
            if chars.get(end) == Some(&'.') && chars.get(end + 1) == Some(&'[') {
                j = end + 1;
            } else {
                break;
            }
        }
        let from = if parts.len() > keep { parts[parts.len() - keep].0 } else { i };
        let to = parts.last().map_or(i + 1, |p| p.1);
        out.extend(&chars[from..to]);
        i = to;
    }
    out
}

fn find(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || from > chars.len() {
        return None;
    }
    chars[from..].windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

/// The index after a quoted run opened at `start`; a doubled closing quote is an escaped one.
fn skip_quoted(chars: &[char], start: usize, close: char, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        if backslash && chars[i] == '\\' {
            i += 2;
        } else if chars[i] == close {
            if chars.get(i + 1) == Some(&close) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    chars.len()
}

fn word_end(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$' | '#' | '@')) {
        i += 1;
    }
    i
}

/// Digits, a decimal point, an exponent, hex (`0x1F`).
fn number_end(chars: &[char], from: usize) -> usize {
    let mut i = from;
    if chars.get(i) == Some(&'0') && matches!(chars.get(i + 1), Some('x' | 'X')) {
        i += 2;
        while i < chars.len() && chars[i].is_ascii_hexdigit() {
            i += 1;
        }
        return i;
    }
    while i < chars.len() {
        let c = chars[i];
        let exponent_sign = matches!(c, '+' | '-') && matches!(chars.get(i.wrapping_sub(1)), Some('e' | 'E'));
        if c.is_ascii_digit() || c == '.' || matches!(c, 'e' | 'E') || exponent_sign {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// A Postgres dollar-quote opener at `start` (`$$` or `$tag$`): the index of its closing `$`. `$1` is not one.
fn dollar_tag(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while i < chars.len() {
        match chars[i] {
            '$' => return Some(i),
            c if c.is_alphanumeric() || c == '_' => i += 1,
            _ => return None,
        }
    }
    None
}

/// Units a plan writes after a measured number: MySQL's `data_read_per_join` ("106M", "1G"), sizes ("8kB", "4MB"),
/// times ("0.5ms") and shares ("10%").
const MEASUREMENT_UNITS: &[&str] = &[
    "K", "M", "G", "T", "P", "E", "B", "KB", "kB", "MB", "GB", "TB", "KiB", "MiB", "GiB", "TiB", "ms", "us", "s", "%",
];

/// A value that is a measurement (cost, rows, a size, a flag), not text that could hold data. A whole value that is a
/// number with a unit is the plan's own figure: a literal from the statement is never only that.
fn is_measurement(value: &str) -> bool {
    let v = value.trim();
    let unit_at = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    let (number, unit) = v.split_at(unit_at);
    v.parse::<f64>().is_ok()
        || (number.parse::<f64>().is_ok() && MEASUREMENT_UNITS.contains(&unit.trim_start()))
        || matches!(v.to_ascii_lowercase().as_str(), "true" | "false" | "yes" | "no")
}

fn redact_props(props: &Properties, engine: Engine) -> Properties {
    props
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                if is_measurement(v) {
                    v.clone()
                } else {
                    let value = redact_sql(v, engine);
                    if engine == Engine::Mssql { strip_database(&value) } else { value }
                },
            )
        })
        .collect()
}

fn redact_plan_expression(text: &str) -> String {
    let mut prefix = "__culprit_plan_reference_".to_string();
    while text.contains(&prefix) {
        prefix.push('_');
    }
    let mut mapped = String::with_capacity(text.len());
    let mut rest = text;
    let mut refs = Vec::new();
    while let Some(at) = rest.find("SubPlan ") {
        let start = at + "SubPlan ".len();
        let digits = rest[start..].chars().take_while(char::is_ascii_digit).count();
        mapped.push_str(&rest[..at]);
        if digits == 0 || rest[..at].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            mapped.push_str("SubPlan ");
            rest = &rest[start..];
            continue;
        }
        let original = &rest[at..start + digits];
        let token = format!("{prefix}{}END", refs.len());
        mapped.push_str(&token);
        refs.push((token, original.to_string()));
        rest = &rest[start + digits..];
    }
    mapped.push_str(rest);
    let mut masked = redact_sql(&mapped, Engine::Postgres);
    for (token, original) in refs {
        masked = masked.replace(&token, &original);
    }
    masked
}

/// The plan with literal values replaced in its statement, predicates and text properties. The original plan text
/// (`raw`) is dropped; use [`redact_native`] for that.
pub fn redact_plan(plan: &Plan) -> Plan {
    fn text(t: &str, engine: Engine) -> String {
        let t = if engine == Engine::Postgres { redact_plan_expression(t) } else { redact_sql(t, engine) };
        if engine == Engine::Mssql { strip_database(&t) } else { t }
    }
    fn node(n: &PlanNode, engine: Engine) -> PlanNode {
        let facts = n.facts.as_ref().map(|facts| {
            let mut f = facts.clone();
            f.selectivity_guess = f.selectivity_guess.as_deref().map(|v| redact_prose(v, engine));
            f.partition_pruning = f.partition_pruning.as_deref().map(|v| redact_prose(v, engine));
            f.join_filter = f.join_filter.as_deref().map(|v| text(v, engine));
            f.storage = f.storage.as_deref().map(|v| redact_strings(v, engine));
            f.join_buffer = f.join_buffer.as_deref().map(|v| redact_strings(v, engine));
            f.join_buffer_join_type = f.join_buffer_join_type.as_deref().map(|v| redact_strings(v, engine));
            f.index_method = f.index_method.as_deref().map(|v| redact_strings(v, engine));
            f.aggregate_strategy = f.aggregate_strategy.as_deref().map(|v| redact_strings(v, engine));
            f.conditions = f.conditions.iter().map(|v| text(v, engine)).collect();
            f.columns_without_statistics = f.columns_without_statistics.iter().map(|v| text(v, engine)).collect();
            f.output_columns = f.output_columns.as_ref().map(|values| values.iter().map(|v| text(v, engine)).collect());
            f.user_functions = f
                .user_functions
                .iter()
                .map(|v| {
                    let value = text(v, engine);
                    if engine == Engine::Mssql { strip_parts(&value, 2) } else { value }
                })
                .collect();
            for c in &mut f.conversions {
                c.expression = text(&c.expression, engine);
                if let crate::PlanConversionIssue::Other(v) = &mut c.issue {
                    *v = redact_strings(v, engine);
                }
            }
            if let Some(crate::PlanParentRelationship::Other(v)) = &mut f.parent_relationship {
                *v = redact_strings(v, engine);
            }
            for mode in [&mut f.actual_execution_mode, &mut f.estimated_execution_mode].into_iter().flatten() {
                if let crate::PlanExecutionMode::Other(v) = mode {
                    *v = redact_strings(v, engine);
                }
            }
            f
        });
        PlanNode {
            facts,
            detail: n.detail.as_deref().map(|d| text(d, engine)),
            seek_predicate: n.seek_predicate.as_deref().map(|p| text(p, engine)),
            predicate: n.predicate.as_deref().map(|p| text(p, engine)),
            props: n
                .props
                .iter()
                .map(|(k, v)| (k.clone(), if is_measurement(v) { v.clone() } else { text(v, engine) }))
                .collect(),
            children: n.children.iter().map(|c| node(c, engine)).collect(),
            ..n.clone()
        }
    }
    let mut redacted = Plan {
        view: None,
        statement: plan.statement.as_deref().map(|s| redact_sql(s, plan.engine)),
        root: node(&plan.root, plan.engine),
        props: redact_props(&plan.props, plan.engine),
        raw: None,
        insights: None,
        ..plan.clone()
    };
    if let Some(f) = &mut redacted.facts {
        f.flavor = f.flavor.as_deref().map(|v| redact_strings(v, plan.engine));
        f.server_version = f.server_version.as_deref().map(|v| redact_strings(v, plan.engine));
        f.memory_grant_warning = f.memory_grant_warning.as_deref().map(|v| redact_strings(v, plan.engine));
        f.compiled_parameters = f.compiled_parameters.as_deref().map(|v| text(v, plan.engine));
        for c in &mut f.conversions {
            c.expression = text(&c.expression, plan.engine);
            if let crate::PlanConversionIssue::Other(v) = &mut c.issue {
                *v = redact_strings(v, plan.engine);
            }
        }
        f.generic_plan_reason = f.generic_plan_reason.as_deref().map(|v| redact_prose(v, plan.engine));
        f.cost_basis = f.cost_basis.as_deref().map(|v| redact_prose(v, plan.engine));
        f.parallelism_blocker = f.parallelism_blocker.as_deref().map(|v| redact_strings(v, plan.engine));
        f.optimization_early_abort = f.optimization_early_abort.as_deref().map(|v| redact_strings(v, plan.engine));
        f.statistics_maintenance = f.statistics_maintenance.as_deref().map(|v| text(v, plan.engine));
    }
    // Insights quote the predicates they were found in: worked out again from the redacted plan.
    if plan.insights.is_some() {
        crate::insights::annotate(&mut redacted);
        // Recomputing from a redacted tree must not restore urgency or unsafe index SQL that the selected scan
        // withheld. Keep primary redaction by deriving prose from the redacted tree; catalog-specific fixes can't
        // be reconstructed from a standalone tree, so those require the original scan's catalog again.
        if let (Some(source), Some(target)) = (&plan.insights, &mut redacted.insights) {
            let original = crate::insights::insights(plan);
            let mut contextual = source.note != crate::insights::coverage_note(plan);
            for i in &mut target.items {
                if let Some(before) =
                    source.items.iter().find(|b| b.rule == i.rule && b.nodes == i.nodes && b.index_hint == i.index_hint)
                {
                    i.severity = i.severity.min(before.severity);
                    i.fix_sql = before.fix_sql.as_deref().map(|sql| redact_sql(sql, plan.engine));
                    if original.iter().any(|raw| {
                        raw.rule == before.rule
                            && raw.nodes == before.nodes
                            && raw.index_hint == before.index_hint
                            && raw.fix != before.fix
                    }) {
                        contextual = true;
                        if i.area == crate::insights::InsightArea::Index {
                            i.fix = "Check this observation against the selected scan's workload and current index definitions before changing an index. Prefer narrow projections and balance saved reads against storage and write cost; a full covering index is not automatically worthwhile.".into();
                        }
                        i.verify = "Compare a newly compiled plan, reads and time under equivalent parameters and load. Stored warnings and a changing share of database time alone do not verify improvement.".into();
                    }
                }
            }
            if contextual {
                let context = "Workload priority and SQL restrictions from the selected scan are retained. Historical or cheap statements can still have structural scaling risks; consult the selected scan for catalog-specific advice.";
                target.note =
                    Some(target.note.as_deref().map_or_else(|| context.to_string(), |n| format!("{n} {context}")));
            }
        }
    }
    redacted
}

/// Showplan XML attributes that carry SQL text or values.
const SHOWPLAN_TEXT: &[&str] = &[
    "StatementText",
    "ScalarString",
    "ParameterCompiledValue",
    "ParameterRuntimeValue",
    "ConstValue",
    "ParameterizedText",
    "Expression",
];

/// The plan's original text (a .sqlplan, EXPLAIN JSON or text) with literal values redacted, keeping its format so it
/// opens in the engine's own tools.
pub fn redact_native(raw: &str, engine: Engine) -> String {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('<') {
        return generic_database(&redact_showplan(raw));
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw)
    {
        redact_json(&mut value, engine);
        return serde_json::to_string_pretty(&value).unwrap_or_default();
    }
    raw.lines().map(|line| redact_text_line(line, engine)).collect::<Vec<_>>().join("\n")
}

fn redact_showplan(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    loop {
        let next = SHOWPLAN_TEXT
            .iter()
            .filter_map(|name| {
                let pattern = format!(" {name}=\"");
                rest.find(&pattern).map(|p| (p, pattern.len()))
            })
            .min_by_key(|(p, _)| *p);
        let Some((start, len)) = next else {
            out.push_str(rest);
            return out;
        };
        let value_start = start + len;
        let Some(value_len) = rest[value_start..].find('"') else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..value_start]);
        let value = xml_unescape(&rest[value_start..value_start + value_len]);
        out.push_str(&xml_escape(&strip_database(&redact_sql(&value, Engine::Mssql))));
        rest = &rest[value_start + value_len..];
    }
}

/// Showplan names the database on every object and column reference (`Database="[shop_prod]"`); a redacted export
/// says `[database]` instead, which SSMS still opens.
fn generic_database(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(p) = rest.find(" Database=\"") {
        let start = p + " Database=\"".len();
        let Some(len) = rest[start..].find('"') else { break };
        out.push_str(&rest[..start]);
        out.push_str("[database]");
        rest = &rest[start + len..];
    }
    out.push_str(rest);
    out
}

fn redact_json(value: &mut serde_json::Value, engine: Engine) {
    match value {
        serde_json::Value::String(s) if !is_measurement(s) => *s = redact_sql(s, engine),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| redact_json(v, engine)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| redact_json(v, engine)),
        _ => {}
    }
}

/// Postgres text EXPLAIN and MySQL tree output: what comes before the `(cost=…)`/`(actual …)` figures, and `Key: value`
/// detail lines, may hold literals; the figures are measurements and stay.
fn redact_text_line(line: &str, engine: Engine) -> String {
    let figures = ["(cost=", "(actual ", "(never executed)", "(rows="].iter().filter_map(|m| line.find(m)).min();
    if let Some(at) = figures {
        return format!("{}{}", redact_sql(&line[..at], engine), &line[at..]);
    }
    let body = line.trim_start();
    if let Some((key, value)) = body.split_once(": ")
        && !key.is_empty()
        && key.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '/')
    {
        const MEASURES: &[&str] = &[
            "Buffers",
            "Planning",
            "Planning Time",
            "Execution Time",
            "Workers Planned",
            "Workers Launched",
            "Sort Method",
            "Heap Blocks",
            "Batches",
            "Memory Usage",
            "I/O Timings",
            "JIT",
            "Functions",
            "Options",
            "Timing",
            "Rows Removed by Filter",
            "Rows Removed by Index Recheck",
            "Rows Removed by Join Filter",
            "Heap Fetches",
            "Peak Memory Usage",
            "Disk Usage",
            "Cache Hits",
            "Cache Misses",
            "Cache Evictions",
            "Hits",
            "Misses",
            "Evictions",
        ];
        if MEASURES.iter().any(|m| m.eq_ignore_ascii_case(key.trim())) {
            return line.to_string();
        }
        let indent = &line[..line.len() - body.len()];
        return format!("{indent}{key}: {}", redact_sql(value, engine));
    }
    redact_strings(line, engine)
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => {
                u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)
            }
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_redacted_snapshot_keeps_no_planted_value() {
        let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture");
        let mut snapshot: Snapshot = serde_json::from_str(&text).expect("snapshot");
        snapshot.tables[0].definition = Some("CREATE TABLE example(value TEXT DEFAULT N'PLANTED-default')".into());
        let w = snapshot.workload.as_mut().expect("workload");
        for q in &mut w.queries {
            q.text = format!("{} AND x = N'PLANTED-text'", q.text);
            for p in &mut q.plans {
                p.compiled = Some("@p0 = N'PLANTED-compiled'".into());
            }
            if let Some(plan) = &mut q.plan {
                plan.root.predicate = Some("[c] = N'PLANTED-predicate'".into());
                plan.props.insert("Parameters".into(), "@p0 = N'PLANTED-props'".into());
                // Insights written from the raw plan, as a collector leaves them.
                crate::insights::annotate(plan);
                if let Some(i) = plan.insights.as_mut() {
                    i.note = Some("seen N'PLANTED-insight'".into());
                }
            }
        }
        let deadlock: crate::deadlock::Deadlock = serde_json::from_value(serde_json::json!({
            "id": "d1", "at": null, "source": "test", "pattern": "cycle", "objects": [], "headline": "N'PLANTED-headline'",
            "explanation": "N'PLANTED-explanation'", "fixes": [], "resources": [],
            "processes": [{"id": "p1", "session": "51", "statement": "UPDATE t SET a = N'PLANTED-deadlock'",
                "procedure": null, "login": "someone@PLANTED-login.com", "host": "PLANTED-host", "application": null,
                "database": null, "isolation": null, "wait_mode": null, "victim": true}]
        }))
        .expect("deadlock");
        snapshot.deadlocks = Some(
            serde_json::from_value(serde_json::json!({
                "deadlocks": [deadlock], "source": "test", "since_hours": 24.0, "counted": 1.0
            }))
            .expect("deadlocks"),
        );
        let out = serde_json::to_string(&redact_snapshot(&snapshot)).expect("json");
        assert!(
            !out.contains("PLANTED"),
            "{}",
            &out[out.find("PLANTED").unwrap_or(0).saturating_sub(200)..][..400.min(out.len())]
        );
    }

    #[test]
    fn prose_keeps_its_words_and_loses_the_values_it_quotes() {
        let summary = "Plan 7 isn't the fast one: it was compiled for `@p0 = N'9b9143e3-90c5-f111-bf5b-000d3aacaa10', @p1 = (42)` \
                       and filters on `[status] = N'active'`.";
        let out = redact_prose(summary, Engine::Mssql);
        assert!(out.starts_with("Plan 7 isn't the fast one"), "{out}");
        assert!(out.contains("compiled for `@p0 = N'?', …`"), "{out}");
        assert!(out.contains("`[status] = N'?'`"), "{out}");
        assert!(!out.contains("9b9143e3") && !out.contains("active") && !out.contains("42"), "{out}");
        // One parameter, or a list that still says something, stays as it is.
        assert_eq!(redact_prose("`@p0 = ?`", Engine::Mssql), "`@p0 = ?`");
        assert_eq!(redact_prose("`nvarchar(4000), @p1 = ?`", Engine::Mssql), "`nvarchar(4000), @p1 = ?`");
    }

    #[test]
    fn a_value_holding_a_backtick_cannot_unmask_the_values_after_it() {
        let out =
            redact_prose("compiled for `@p0 = N'a`b', @p1 = N'secret', @p2 = 4242`. Then `x = 'y'`.", Engine::Mssql);
        assert!(!out.contains("secret") && !out.contains("4242") && !out.contains("a`b"), "{out}");
        // A literal that never closes hides everything after it.
        let out = redact_prose("compiled for `@p0 = 1, @p1 = N'secret and more", Engine::Mssql);
        assert!(!out.contains("secret") && !out.contains("more"), "{out}");
        // A doubled quote is part of its value.
        let out = redact_prose("`@p0 = N'it''s secret'` done", Engine::Mssql);
        assert!(!out.contains("secret") && out.ends_with("done"), "{out}");
        // GUIDs go wherever they are; apostrophes and words stay.
        let out = redact_prose("tenant 9b9143e3-90c5-f111-bf5b-000d3aacaa10 isn't the plan's problem", Engine::Mssql);
        assert_eq!(out, "tenant ? isn't the plan's problem");
        // MySQL escapes quotes with backslashes.
        let out = redact_prose(r"`status = 'it\'s `secret`'` done", Engine::Mysql);
        assert!(!out.contains("secret"), "{out}");
    }

    use super::*;

    #[test]
    fn sql_server_literals_go_names_and_markers_stay() {
        let r = redact_sql(
            "SELECT TOP (5) [id] FROM [shop].[orders] WHERE [email] = N'ann@example.com' AND total > 12.5 AND id = @P1 -- vip\n",
            Engine::Mssql,
        );
        assert_eq!(
            r,
            "SELECT TOP (?) [id] FROM [shop].[orders] WHERE [email] = N'?' AND total > ? AND id = @P1 -- …\n"
        );
        assert_eq!(
            redact_sql(
                "[db].[shop].[customers].[email]=CONVERT_IMPLICIT(nvarchar(200),[@P1],0) AND [x]=(42)",
                Engine::Mssql
            ),
            "[db].[shop].[customers].[email]=CONVERT_IMPLICIT(nvarchar(200),[@P1],?) AND [x]=(?)"
        );
        assert_eq!(redact_sql("WHERE [it's] = 'it''s' AND b = 0x1F", Engine::Mssql), "WHERE [it's] = '?' AND b = ?");
        // A LIKE pattern keeps its wildcard shape, never its text.
        assert_eq!(
            redact_sql("email LIKE '%@example.com' AND name LIKE 'Al%' AND note LIKE '%x%' AND c = '%'", Engine::Mysql),
            "email LIKE '%?' AND name LIKE '?%' AND note LIKE '%?%' AND c = '%?'"
        );
        assert_eq!(redact_sql("WHERE n LIKE N'%smith'", Engine::Mssql), "WHERE n LIKE N'%?'");
        assert_eq!(
            redact_sql("CAST(a AS decimal(12,2)) /* customer 42 */", Engine::Mssql),
            "CAST(a AS decimal(12,2)) /* … */"
        );
    }

    #[test]
    fn postgres_literals_go_names_and_markers_stay() {
        assert_eq!(
            redact_sql("SELECT \"Id\" FROM orders WHERE status = 'open'::text AND id = $1 AND n > 3", Engine::Postgres),
            "SELECT \"Id\" FROM orders WHERE status = '?'::text AND id = $1 AND n > ?"
        );
        assert_eq!(
            redact_sql("note = E'it\\'s' OR note = $$a 'b' c$$ OR x = $tag$q$tag$", Engine::Postgres),
            "note = E'?' OR note = '?' OR x = '?'"
        );
        assert_eq!(
            redact_sql("created_at > (now() - '7 days'::interval)", Engine::Postgres),
            "created_at > (now() - '?'::interval)"
        );
        assert_eq!(redact_sql("name::character varying(20)", Engine::Postgres), "name::character varying(20)");
    }

    #[test]
    fn mysql_literals_go_names_and_markers_stay() {
        assert_eq!(
            redact_sql(
                "SELECT `id` FROM orders WHERE note = \"a \\\" b\" AND s = 'x' AND id = ? AND n = 7 # tail",
                Engine::Mysql
            ),
            "SELECT `id` FROM orders WHERE note = \"?\" AND s = '?' AND id = ? AND n = ? # …"
        );
        assert_eq!(
            redact_sql("(`shop`.`orders`.`customer_id` = 7)", Engine::Mysql),
            "(`shop`.`orders`.`customer_id` = ?)"
        );
        assert_eq!(redact_sql("SELECT 1--2", Engine::Mysql), "SELECT ?--?");
        assert_eq!(redact_sql("SELECT /*+ NO_INDEX(o) */ 1", Engine::Mysql), "SELECT /*+ NO_INDEX(o) */ ?");
    }

    #[test]
    fn database_names_drop_from_four_part_names() {
        assert_eq!(
            strip_database("[shop_prod].[dbo].[orders].[id]=[@P1] AND [dbo].[orders].[x]=1 AND [a]"),
            "[dbo].[orders].[id]=[@P1] AND [dbo].[orders].[x]=1 AND [a]"
        );
        assert_eq!(
            strip_database("CONVERT_IMPLICIT(nvarchar(200),[db].[s].[t].[c],0)"),
            "CONVERT_IMPLICIT(nvarchar(200),[s].[t].[c],0)"
        );
    }

    #[test]
    fn identifiers_with_digits_survive() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql] {
            assert_eq!(redact_sql("SELECT t1.col2 FROM ix_2024 t1", engine), "SELECT t1.col2 FROM ix_2024 t1");
        }
    }

    #[test]
    fn showplan_attributes_are_redacted_and_stay_valid_xml() {
        let xml = r#"<StmtSimple StatementText="SELECT a FROM t WHERE b = N&apos;x&apos; AND c = 5" StatementId="1"><ColumnReference Column="@p1" ParameterCompiledValue="N&apos;abc&apos;" /><ScalarOperator ScalarString="[t].[b]=(42)"/></StmtSimple>"#;
        let out = redact_native(xml, Engine::Mssql);
        assert!(out.contains(r#"StatementText="SELECT a FROM t WHERE b = N&apos;?&apos; AND c = ?""#), "{out}");
        assert!(out.contains(r#"ParameterCompiledValue="N&apos;?&apos;""#));
        assert!(out.contains(r#"ScalarString="[t].[b]=(?)""#));
        assert!(out.contains(r#"StatementId="1""#));
        let named = r#"<Object Database="[shop_prod]" Schema="[dbo]" Table="[orders]" /><ScalarOperator ScalarString="[shop_prod].[dbo].[orders].[id]=(1)"/>"#;
        let out = redact_native(named, Engine::Mssql);
        assert!(!out.contains("shop_prod"), "{out}");
        assert!(out.contains(r#"Database="[database]""#) && out.contains(r#"ScalarString="[dbo].[orders].[id]=(?)""#));
    }

    #[test]
    fn text_plans_keep_their_figures() {
        let pg = "Index Scan using ix on orders o  (cost=0.42..8.44 rows=1 width=4)\n  Index Cond: (customer_id = 42)\n  Filter: (status = 'open'::text)\n  Rows Removed by Filter: 12\nPlanning Time: 0.2 ms";
        let out = redact_native(pg, Engine::Postgres);
        assert!(out.contains("(cost=0.42..8.44 rows=1 width=4)"));
        assert!(out.contains("Index Cond: (customer_id = ?)"));
        assert!(out.contains("Filter: (status = '?'::text)"));
        assert!(out.contains("Rows Removed by Filter: 12"));
        assert!(out.contains("Planning Time: 0.2 ms"));
        let my = "-> Filter: (o.customer_id = 7)  (cost=0.35 rows=1) (actual time=0.1..0.1 rows=1 loops=1)";
        assert_eq!(
            redact_native(my, Engine::Mysql),
            "-> Filter: (o.customer_id = ?)  (cost=0.35 rows=1) (actual time=0.1..0.1 rows=1 loops=1)"
        );
    }

    #[test]
    fn json_plans_keep_measurements() {
        let raw = r#"{"query_block": {"cost_info": {"query_cost": "1.25"}, "table": {"table_name": "orders", "attached_condition": "(`o`.`status` = 'open')"}}}"#;
        let out = redact_native(raw, Engine::Mysql);
        assert!(out.contains("\"query_cost\": \"1.25\""));
        assert!(out.contains("(`o`.`status` = '?')"));
        assert!(out.contains("\"table_name\": \"orders\""));
    }

    // A query hint's number tunes the plan: the parallelism insight reads `OPTION (MAXDOP 1)` in a redacted statement
    // as it does in the original.
    #[test]
    fn query_hints_keep_their_numbers() {
        assert_eq!(
            redact_sql("SELECT * FROM t WHERE id = 5 OPTION (MAXDOP 1, FAST 10)", Engine::Mssql),
            "SELECT * FROM t WHERE id = ? OPTION (MAXDOP 1, FAST 10)"
        );
        assert_eq!(redact_sql("SELECT TOP 5 x FROM t WHERE y > 3", Engine::Mssql), "SELECT TOP ? x FROM t WHERE y > ?");
    }

    // MySQL writes sizes with a unit ("106M" read per join): a measurement, not a literal to mask as "?M".
    #[test]
    fn a_number_with_a_unit_is_a_measurement() {
        let raw = r#"{"query_block": {"table": {"table_name": "orders", "cost_info": {"data_read_per_join": "106M", "read_cost": "12.50"}, "attached_condition": "(`o`.`code` = '106M')"}}}"#;
        let out = redact_native(raw, Engine::Mysql);
        assert!(out.contains("\"data_read_per_join\": \"106M\""), "{out}");
        assert!(out.contains("(`o`.`code` = '?')"), "{out}");
        for kept in ["106M", "3.5K", "1G", "8kB", "0.5 ms", "10%", "1.25"] {
            assert!(is_measurement(kept), "{kept}");
        }
        for masked in ["106 rows", "M106", "abc", "10M'", "'106M'"] {
            assert!(!is_measurement(masked), "{masked}");
        }
    }

    #[test]
    fn endpoint_tokens_are_repeatable_and_never_merge_different_databases() {
        use crate::{DatabaseIdentity, DatabaseLocation};
        for location in [
            DatabaseLocation::Server {
                host: "private-endpoint.example".into(),
                port: 5432,
                instance: Some("private-instance".into()),
            },
            DatabaseLocation::Socket { directory: "/private/socket-directory".into(), port: 5432 },
            DatabaseLocation::File { path: "/Users/private-person/database.sqlite".into() },
            DatabaseLocation::Unknown,
        ] {
            let original = DatabaseIdentity {
                engine: Engine::Postgres,
                database: "catalog_label".into(),
                location,
                incarnation: Some("private-incarnation".into()),
            };
            let mut redacted = original.clone();
            redact_identity(&mut redacted);
            let once = redacted.clone();
            redact_identity(&mut redacted);
            assert_eq!(once, redacted);
            assert!(once.comparison(&redacted).allows_comparison());
            let encoded = serde_json::to_string(&redacted).unwrap();
            for secret in
                ["private-endpoint", "private-instance", "private-person", "socket-directory", "private-incarnation"]
            {
                assert!(!encoded.contains(secret));
            }
            let mut different = original;
            different.incarnation = Some("another-incarnation".into());
            redact_identity(&mut different);
            assert!(!redacted.comparison(&different).allows_comparison());
        }
        let mut invalid = DatabaseIdentity::server(Engine::Postgres, "label", "", 0, None);
        redact_identity(&mut invalid);
        assert!(!invalid.comparison(&invalid).allows_comparison());
    }

    #[test]
    fn sharing_keeps_public_deadlock_limits_without_private_collector_details() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        for (engine, public_detail) in [
            (Engine::Postgres, "server log"),
            (Engine::Mysql, "most recent deadlock"),
            (Engine::Mssql, "details omitted"),
        ] {
            snapshot.server.engine = engine;
            snapshot.deadlocks = Some(crate::DeadlockReport {
                note: Some("Collection failed on private-collector-endpoint".into()),
                ..Default::default()
            });
            let shared = redact_snapshot(&snapshot);
            let note = shared.deadlocks.as_ref().unwrap().note.as_deref().unwrap();
            assert!(note.contains(public_detail));
            assert!(!note.contains("private-collector-endpoint"));
            assert_eq!(redact_snapshot(&shared).deadlocks.as_ref().unwrap().note.as_deref(), Some(note));
            snapshot.deadlocks.as_mut().unwrap().note = None;
            assert!(redact_snapshot(&snapshot).deadlocks.unwrap().note.is_none());
        }
    }

    #[test]
    fn shared_snapshots_scrub_catalog_literals_and_all_retained_endpoint_references() {
        use crate::{BaselinePurpose, BaselineReference, CountBaseline, ScanReference};
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        snapshot.bind_identity(crate::DatabaseIdentity::server(
            Engine::Postgres,
            &snapshot.server.database.clone(),
            "private-endpoint-canary",
            5432,
            None,
        ));
        snapshot.ensure_measurement();
        let reference = Some(ScanReference::of(&snapshot));
        let measurement = snapshot.workload.as_ref().unwrap().measurement.clone();
        snapshot.workload.as_mut().unwrap().history_inputs.push(BaselineReference {
            purpose: BaselinePurpose::WorkloadTrend,
            scan: reference.clone().unwrap(),
            measurement: measurement.clone(),
        });
        snapshot.escalations_before =
            Some(crate::EscalationBaseline { reference: reference.clone(), ..Default::default() });
        snapshot.contention_before =
            Some(crate::ContentionBaseline { reference: reference.clone(), ..Default::default() });
        snapshot.evictions_before =
            Some(CountBaseline { reference: reference.clone(), measurement, ..Default::default() });
        snapshot.sizes_before = Some(crate::SizeBaseline { reference: reference.clone(), ..Default::default() });
        snapshot.operations.sqlite = Some(crate::operations::SqliteFileHealth {
            wal_bytes: Some(100.0),
            before: Some(crate::operations::CounterReading { at: snapshot.taken_at.clone(), count: 50.0, reference }),
        });
        snapshot.indexes[0].filter = Some("tenant = 'literal-value-canary'".into());
        snapshot.indexes[0].definition =
            Some("CREATE INDEX ix ON public.orders (id) WHERE tenant = 'literal-value-canary'".into());
        snapshot.columns[0].default = Some("'literal-value-canary'".into());
        snapshot.tuning = Some(crate::TuningAdvice {
            recommendations: vec![crate::TuningRecommendation {
                script: Some("SELECT 'literal-value-canary'".into()),
                reason: Some("Value 'literal-value-canary' was observed".into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let shared = redact_snapshot(&snapshot);
        let output = serde_json::to_string(&shared).unwrap();
        assert!(!output.contains("private-endpoint-canary"));
        assert!(!output.contains("literal-value-canary"));
        assert_eq!(shared.server.database, snapshot.server.database, "database labels are intentional digest metadata");
        assert_eq!(shared.evictions_before.as_ref().unwrap().count, snapshot.evictions_before.as_ref().unwrap().count);
        assert_eq!(serde_json::to_value(&shared).unwrap(), serde_json::to_value(redact_snapshot(&shared)).unwrap());
    }

    #[test]
    fn plan_facts_and_plugin_fix_sql_share_the_same_redaction_boundary() {
        let mut report: crate::Report =
            serde_json::from_str(include_str!("../../../fixtures/mssql-demo.report.json")).unwrap();
        let plan = report.snapshot.workload.as_mut().unwrap().queries.iter_mut().find_map(|q| q.plan.as_mut()).unwrap();
        let value = "[plan-database-canary].[dbo].[orders].[tenant] = 'literal-value-canary'";
        plan.props.insert("PlanAffectingConvert".into(), value.into());
        let facts = plan.root.facts.get_or_insert_default();
        facts.output_columns = Some(vec![value.into()]);
        facts.columns_without_statistics = vec![value.into()];
        facts.user_functions = vec!["[plan-database-canary].[dbo].[function_name]".into()];
        let redacted = serde_json::to_string(&redact_plan(plan)).unwrap();
        assert!(!redacted.contains("plan-database-canary"));
        assert!(!redacted.contains("literal-value-canary"));
        let finding = &mut report.findings[0];
        finding.title = "Value 'literal-value-canary'".into();
        finding.fix = Some(crate::Fix {
            summary: "Use the predicate".into(),
            sql: Some("CREATE INDEX ix ON dbo.t (x) WHERE tenant = 'literal-value-canary'".into()),
            cost: None,
        });
        assert!(
            !serde_json::to_string(&redact_finding(finding, Engine::Mssql)).unwrap().contains("literal-value-canary")
        );
    }
}
