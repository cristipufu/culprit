//! Offline migration and DDL SQL linter against a saved [`Snapshot`].
//!
//! Checks a proposed migration script or DDL statement against the real table sizes, row counts, indexes and column
//! types in a saved snapshot without touching the database:
//! - Blocking `CREATE INDEX` without `CONCURRENTLY` (PostgreSQL) or `ONLINE = ON` (SQL Server) on large tables.
//! - `ALTER TABLE ... ADD FOREIGN KEY` where the referencing columns have no supporting index in the snapshot (or in
//!   the migration itself) or have a column type mismatch with the referenced parent column.
//! - `ALTER TABLE` operations that hold `ACCESS EXCLUSIVE` / schema-modification locks while scanning or rewriting a
//!   large table (`ALTER COLUMN ... TYPE`, `SET NOT NULL`).

use culprit_core::{
    Actionability, Category, Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, SubjectKind, TableStat,
    Unit, format,
};

use crate::support::{base_score, finding, subject, supports_online_index};

const LARGE_TABLE_ROWS: f64 = 100_000.0;
const HIGH_TABLE_ROWS: f64 = 1_000_000.0;
const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = 1024.0 * MB;
const LARGE_TABLE_BYTES: f64 = 100.0 * MB;

struct ScriptContext<'a> {
    created_indexes: &'a [(String, Vec<String>)],
    validated_tables: &'a [String],
    in_transaction: bool,
}

/// Lint one or more SQL statements (such as a migration file) against the catalog and table sizes in `snapshot`.
pub fn lint_sql(snapshot: &Snapshot, sql: &str) -> Vec<Finding> {
    let statements = split_statements(sql);
    let created_indexes = collect_script_indexes(&statements);
    let validated_tables = collect_validated_constraint_tables(&statements);
    let mut findings = Vec::new();
    let mut in_transaction = false;
    for stmt in &statements {
        let ctx =
            ScriptContext { created_indexes: &created_indexes, validated_tables: &validated_tables, in_transaction };
        lint_single_statement(snapshot, stmt, &ctx, &mut in_transaction, &mut findings);
    }
    findings
}

/// Apply all safe DDL auto-fixes (`Fix.sql`) recommended by [`lint_sql`] to `sql`, returning `(rewritten_sql, fixes_applied)`.
pub fn apply_sql_fixes(snapshot: &Snapshot, sql: &str) -> (String, usize) {
    let spans = split_statement_spans(sql);
    if spans.is_empty() {
        return (sql.to_string(), 0);
    }
    let statements: Vec<String> = spans.iter().map(|(_, _, s)| s.clone()).collect();
    let mut created_indexes = collect_script_indexes(&statements);
    let validated_tables = collect_validated_constraint_tables(&statements);

    let mut out = String::with_capacity(sql.len() + 128);
    let mut last_pos = 0usize;
    let mut applied = 0usize;
    let mut in_transaction = false;

    for (start, end, stmt) in &spans {
        let mut stmt_findings = Vec::new();
        {
            let ctx = ScriptContext {
                created_indexes: &created_indexes,
                validated_tables: &validated_tables,
                in_transaction,
            };
            lint_single_statement(snapshot, stmt, &ctx, &mut in_transaction, &mut stmt_findings);
        }

        out.push_str(&sql[last_pos..*start]);

        for f in &stmt_findings {
            if f.id.starts_with("sql-lint:unindexed-fk:")
                && !(in_transaction && snapshot.server.engine == Engine::Postgres)
                && let Some(fix_sql) = f.fix.as_ref().and_then(|fix| fix.sql.as_deref())
            {
                let trimmed = fix_sql.trim();
                out.push_str(trimmed);
                out.push('\n');
                created_indexes.extend(collect_script_indexes(&split_statements(trimmed)));
                applied += 1;
            }
        }

        let replacements: Vec<&str> = stmt_findings
            .iter()
            .filter(|f| !f.id.starts_with("sql-lint:unindexed-fk:"))
            .filter_map(|f| f.fix.as_ref().and_then(|fix| fix.sql.as_deref()))
            .collect();

        if let [rep_sql] = replacements.as_slice() {
            out.push_str(&rewrite_preserving_comments(&sql[*start..*end], rep_sql.trim()));
            applied += 1;
        } else {
            out.push_str(&sql[*start..*end]);
        }
        last_pos = *end;
    }
    out.push_str(&sql[last_pos..]);
    (out, applied)
}

fn lint_single_statement(
    snapshot: &Snapshot,
    stmt: &str,
    ctx: &ScriptContext<'_>,
    in_transaction: &mut bool,
    findings: &mut Vec<Finding>,
) {
    let tokens = tokenize(stmt);
    if tokens.is_empty() {
        return;
    }
    let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
    if is_transaction_begin(&upper) {
        *in_transaction = true;
        return;
    }
    if is_transaction_end(&upper) {
        *in_transaction = false;
        return;
    }
    check_concurrent_in_tx(snapshot, stmt, &tokens, &upper, *in_transaction, findings);
    check_create_index(snapshot, stmt, &tokens, *in_transaction, findings);
    check_drop_or_truncate(snapshot, stmt, &tokens, &upper, *in_transaction, findings);
    check_maintenance(snapshot, stmt, &tokens, &upper, *in_transaction, findings);
    check_alter_table(snapshot, stmt, &tokens, ctx, findings);
}

fn is_transaction_begin(upper: &[String]) -> bool {
    (upper.first().map(String::as_str) == Some("BEGIN")
        && matches!(
            upper.get(1).map(String::as_str),
            None | Some(
                "WORK"
                    | "TRANSACTION"
                    | "TRAN"
                    | "ISOLATION"
                    | "READ"
                    | "DEFERRABLE"
                    | "NOT"
                    | "DEFERRED"
                    | "IMMEDIATE"
                    | "EXCLUSIVE"
                    | "DISTRIBUTED"
            )
        ))
        || (upper.first().map(String::as_str) == Some("START")
            && upper.get(1).map(String::as_str) == Some("TRANSACTION"))
}

fn is_transaction_end(upper: &[String]) -> bool {
    matches!(upper.first().map(String::as_str), Some("COMMIT" | "ROLLBACK" | "END"))
}

fn check_concurrent_in_tx(
    snapshot: &Snapshot,
    stmt: &str,
    tokens: &[String],
    upper: &[String],
    in_transaction: bool,
    findings: &mut Vec<Finding>,
) {
    if !in_transaction || snapshot.server.engine != Engine::Postgres {
        return;
    }
    let Some(conc_pos) = upper.iter().position(|t| t == "CONCURRENTLY") else {
        return;
    };
    let head = upper.first().map(String::as_str).unwrap_or("");
    if !matches!(head, "CREATE" | "DROP" | "REINDEX") {
        return;
    }
    let target = tokens
        .get(conc_pos + 1)
        .filter(|t| !t.eq_ignore_ascii_case("IF") && !t.eq_ignore_ascii_case("ON"))
        .or_else(|| tokens.get(conc_pos + 3))
        .map(|s| unquote_ident(s))
        .unwrap_or_else(|| "index".into());
    let kind_word = if head == "REINDEX" && upper[..conc_pos].iter().any(|t| t == "TABLE") { "TABLE" } else { "INDEX" };
    let mut f = finding(
        "sql-lint",
        &format!("concurrent-in-tx:{target}"),
        Category::Indexes,
        subject(SubjectKind::Index, &target),
    );
    f.actionability = Actionability::NeedsAttention;
    f.confidence = Confidence::High;
    f.severity = Severity::High;
    f.impact.score = base_score(f.severity);
    f.impact.label = "aborts in transaction".into();
    f.title = format!("{head} {kind_word} CONCURRENTLY ({target}) cannot run inside a transaction block");
    f.summary = format!(
        "The statement `{}` uses `CONCURRENTLY` inside an explicit `BEGIN ... COMMIT` transaction block. PostgreSQL \
         immediately aborts `CREATE INDEX CONCURRENTLY`, `DROP INDEX CONCURRENTLY` and `REINDEX CONCURRENTLY` when \
         executed inside a transaction block.",
        compact_sql(stmt)
    );
    f.fix = Some(Fix {
        summary: "Run the `CONCURRENTLY` statement outside `BEGIN ... COMMIT` (for example `disable_ddl_transaction!` in Rails, `atomic = False` in Django, or `-- migrate:no-transaction`).".into(),
        sql: None,
        cost: None,
    });
    f.verify = vec!["Remove the surrounding `BEGIN`/`COMMIT` and re-run `culprit lint-sql`.".into()];
    findings.push(f);
}

fn check_drop_or_truncate(
    snapshot: &Snapshot,
    stmt: &str,
    tokens: &[String],
    upper: &[String],
    in_transaction: bool,
    findings: &mut Vec<Finding>,
) {
    match upper.first().map(String::as_str) {
        Some("DROP") if upper.get(1).map(String::as_str) == Some("INDEX") => {
            let engine = snapshot.server.engine;
            let is_blocking = match engine {
                Engine::Postgres => !upper.iter().any(|t| t == "CONCURRENTLY"),
                Engine::Mysql => !has_mysql_online_ddl(upper),
                _ => false,
            };
            if !is_blocking {
                return;
            }
            let mut pos = 2;
            if upper.get(pos).map(String::as_str) == Some("IF")
                && upper.get(pos + 1).map(String::as_str) == Some("EXISTS")
            {
                pos += 2;
            }
            let Some(raw_idx) = tokens.get(pos) else { return };
            let idx_clean = unquote_ident(raw_idx);
            let idx_leaf = idx_clean.rsplit('.').next().unwrap_or(&idx_clean);
            let on_table = upper
                .iter()
                .position(|t| t == "ON")
                .and_then(|on_pos| tokens.get(on_pos + 1))
                .and_then(|raw_tbl| find_table(snapshot, raw_tbl));
            let Some(ix) = snapshot.indexes.iter().find(|i| {
                i.name.eq_ignore_ascii_case(idx_leaf)
                    && on_table.is_none_or(|t| {
                        i.schema.eq_ignore_ascii_case(&t.schema) && i.table.eq_ignore_ascii_case(&t.name)
                    })
            }) else {
                return;
            };
            let Some(table) = snapshot.table(&ix.schema, &ix.table).filter(|t| is_large_table(t)) else {
                return;
            };
            let rows = table_rows(table);
            let bytes = table_bytes(table);
            let qual = table.qualified();
            let mut f = finding(
                "sql-lint",
                &format!("blocking-drop-index:{}", ix.name),
                Category::Indexes,
                subject(SubjectKind::Index, format!("{qual}.{}", ix.name)),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            if engine == Engine::Mysql {
                f.title = format!(
                    "DROP INDEX {} on {} ({}) without LOCK=NONE can block concurrent writes",
                    ix.name,
                    table.name,
                    table_size_label(rows, bytes)
                );
                f.summary = format!(
                    "The statement `{}` drops `{}` on {} ({}) without `ALGORITHM=INPLACE LOCK=NONE`. Specifying \
                     `ALGORITHM=INPLACE, LOCK=NONE` guarantees an online drop that never falls back to a blocking \
                     table lock.",
                    compact_sql(stmt),
                    ix.name,
                    qual,
                    table_size_label(rows, bytes)
                );
                f.fix = Some(Fix {
                    summary: "Drop the index with `ALGORITHM=INPLACE, LOCK=NONE` so concurrent reads and writes are not blocked.".into(),
                    sql: Some(Engine::Mysql.dialect().drop_index(&ix.schema, &ix.table, &ix.name)),
                    cost: None,
                });
                f.verify = vec!["Re-run `culprit lint-sql` after adding `ALGORITHM=INPLACE, LOCK=NONE`.".into()];
            } else {
                f.title = format!(
                    "DROP INDEX {} on {} ({}) without CONCURRENTLY acquires an ACCESS EXCLUSIVE lock",
                    ix.name,
                    table.name,
                    table_size_label(rows, bytes)
                );
                f.summary = format!(
                    "The statement `{}` drops `{}` on {} ({}) without `CONCURRENTLY`. Standard `DROP INDEX` in PostgreSQL \
                     acquires an `ACCESS EXCLUSIVE` lock on the table, blocking all concurrent reads and writes until \
                     in-flight queries finish.",
                    compact_sql(stmt),
                    ix.name,
                    qual,
                    table_size_label(rows, bytes)
                );
                let multi_drop = upper.iter().any(|t| t == ",");
                let q_idx = postgres_qualified_index(&ix.schema, &ix.name);
                f.fix = Some(Fix {
                    summary: "Use `DROP INDEX CONCURRENTLY` (outside a transaction block) so concurrent reads and writes are not blocked.".into(),
                    sql: (!in_transaction && !multi_drop)
                        .then(|| format!("DROP INDEX CONCURRENTLY IF EXISTS {q_idx};")),
                    cost: Some("DROP INDEX CONCURRENTLY waits for active transactions using the index and cannot run inside a transaction block.".into()),
                });
                f.verify = vec!["Re-run `culprit lint-sql` after switching to `DROP INDEX CONCURRENTLY`.".into()];
            }
            push_table_metrics(&mut f, rows, bytes);
            findings.push(f);
        }
        Some("DROP") if upper.get(1).map(String::as_str) == Some("TABLE") => {
            let mut pos = 2;
            if upper.get(pos).map(String::as_str) == Some("IF")
                && upper.get(pos + 1).map(String::as_str) == Some("EXISTS")
            {
                pos += 2;
            }
            if let Some(raw_table) = tokens.get(pos)
                && let Some(table) = find_table(snapshot, raw_table)
            {
                check_workload_references(snapshot, stmt, table, None, "DROP TABLE", findings);
            }
        }
        Some("TRUNCATE") => {
            let mut pos = 1;
            if upper.get(pos).map(String::as_str) == Some("TABLE") {
                pos += 1;
            }
            if upper.get(pos).map(String::as_str) == Some("ONLY") {
                pos += 1;
            }
            let Some(raw_table) = tokens.get(pos) else { return };
            let Some(table) = find_table(snapshot, raw_table).filter(|t| is_large_table(t)) else {
                return;
            };
            let rows = table_rows(table);
            let bytes = table_bytes(table);
            let qual = table.qualified();
            let mut f = finding(
                "sql-lint",
                &format!("truncate:{qual}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = Severity::High;
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "TRUNCATE on {} ({}) acquires an exclusive table lock and deletes all rows",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` truncates {} ({}). `TRUNCATE` acquires an `ACCESS EXCLUSIVE` / schema-modification \
                 lock on the table, blocking all concurrent queries while deallocating its storage.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec!["Confirm `TRUNCATE` is intended for this environment before running.".into()];
            findings.push(f);
        }
        _ => {}
    }
}

fn check_workload_references(
    snapshot: &Snapshot,
    stmt: &str,
    table: &TableStat,
    column: Option<&str>,
    action: &str,
    findings: &mut Vec<Finding>,
) {
    let active: Vec<&culprit_core::QueryStat> = crate::support::queries_touching(snapshot, &table.schema, &table.name)
        .into_iter()
        .filter(|q| {
            !crate::support::is_monitoring_query(q)
                && !crate::query_is_history(snapshot, q)
                && column.is_none_or(|col| contains_identifier(&q.text, col))
        })
        .collect();
    if active.is_empty() {
        return;
    }
    let qual = table.qualified();
    let target = match column {
        Some(col) => format!("{qual}.{col}"),
        None => qual.clone(),
    };
    let target_short = match column {
        Some(col) => format!("{}.{col}", table.name),
        None => table.name.clone(),
    };
    let mut f =
        finding("sql-lint", &format!("workload-break:{target}"), Category::Queries, subject(SubjectKind::Table, &qual));
    f.actionability = Actionability::NeedsAttention;
    f.confidence = Confidence::High;
    f.severity = Severity::High;
    f.impact.score = base_score(f.severity);
    f.impact.label = format!("{} active {}", active.len(), if active.len() == 1 { "query" } else { "queries" });
    f.queries = crate::support::query_refs(&active);
    f.title = format!(
        "{action} on {target_short} breaks {} active workload {}",
        active.len(),
        if active.len() == 1 { "query" } else { "queries" }
    );
    f.summary = format!(
        "The migration statement `{}` runs `{action}` on `{target_short}`, which is referenced by {} active \
         {} in the captured workload. Deploying this DDL while the current application code is running will cause \
         immediate query errors.",
        compact_sql(stmt),
        active.len(),
        if active.len() == 1 { "statement" } else { "statements" }
    );
    f.fix = Some(Fix {
        summary: format!(
            "Stop reading/writing `{target_short}` in application code and deploy that change before running `{action}` in a later migration."
        ),
        sql: None,
        cost: None,
    });
    f.verify = vec![format!(
        "Verify no active queries in `culprit top` reference `{target_short}` before applying `{action}`."
    )];
    findings.push(f);
}

fn contains_identifier(sql_text: &str, ident: &str) -> bool {
    let lower_sql = sql_text.to_ascii_lowercase();
    let lower_ident = ident.to_ascii_lowercase();
    if lower_ident.is_empty() {
        return false;
    }
    lower_sql.match_indices(&lower_ident).any(|(at, _)| {
        let is_word = |ch: Option<char>| ch.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let before = lower_sql[..at].chars().last();
        let after = lower_sql[at + lower_ident.len()..].chars().next();
        !is_word(before) && !is_word(after)
    })
}

fn check_maintenance(
    snapshot: &Snapshot,
    stmt: &str,
    tokens: &[String],
    upper: &[String],
    in_transaction: bool,
    findings: &mut Vec<Finding>,
) {
    if snapshot.server.engine != Engine::Postgres {
        return;
    }
    match upper.first().map(String::as_str) {
        Some("VACUUM") => {
            let mut pos = 1;
            let mut is_full = false;
            if upper.get(pos).map(String::as_str) == Some("(") {
                let mut depth = 0usize;
                for (i, tok) in upper.iter().enumerate().skip(pos) {
                    match tok.as_str() {
                        "(" => depth += 1,
                        ")" => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                pos = i + 1;
                                break;
                            }
                        }
                        "FULL" if depth == 1 => {
                            is_full = true;
                        }
                        _ => {}
                    }
                }
            } else {
                while let Some(tok) = upper.get(pos).map(String::as_str) {
                    if tok == "FULL" {
                        is_full = true;
                        pos += 1;
                    } else if matches!(tok, "FREEZE" | "VERBOSE" | "ANALYZE" | "ANALYSE") {
                        pos += 1;
                    } else {
                        break;
                    }
                }
            }
            if !is_full {
                return;
            }
            let Some(raw_table) = tokens.get(pos) else { return };
            let Some(table) = find_table(snapshot, raw_table).filter(|t| is_large_table(t)) else {
                return;
            };
            let rows = table_rows(table);
            let bytes = table_bytes(table);
            let qual = table.qualified();
            let mut f = finding(
                "sql-lint",
                &format!("vacuum-full:{qual}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = Severity::High;
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "VACUUM FULL on {} ({}) rewrites the entire table under an ACCESS EXCLUSIVE lock",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` runs `VACUUM FULL` on {} ({}). Unlike standard `VACUUM`, `VACUUM FULL` rewrites \
                 every row and index into new disk files while holding an `ACCESS EXCLUSIVE` lock, blocking all \
                 concurrent reads and writes.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            f.fix = Some(Fix {
                summary: "Run standard `VACUUM (ANALYZE)` (which reclaims dead tuples online without blocking reads or writes), or use `pg_repack` if physical table compaction is required.".into(),
                sql: Some(format!("VACUUM (ANALYZE) {qual};")),
                cost: None,
            });
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec!["Replace `VACUUM FULL` with `VACUUM (ANALYZE)` or `pg_repack`.".into()];
            findings.push(f);
        }
        Some("CLUSTER") => {
            let mut pos = 1;
            if upper.get(pos).map(String::as_str) == Some("VERBOSE") {
                pos += 1;
            }
            let Some(raw_table) = tokens.get(pos) else { return };
            let Some(table) = find_table(snapshot, raw_table).filter(|t| is_large_table(t)) else {
                return;
            };
            let rows = table_rows(table);
            let bytes = table_bytes(table);
            let qual = table.qualified();
            let mut f = finding(
                "sql-lint",
                &format!("cluster:{qual}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = Severity::High;
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "CLUSTER on {} ({}) rewrites the table under an ACCESS EXCLUSIVE lock",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` runs `CLUSTER` on {} ({}). `CLUSTER` physically reorders and rewrites the table \
                 while holding an `ACCESS EXCLUSIVE` lock, blocking all concurrent reads and writes.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            f.fix = Some(Fix {
                summary: "Use `pg_repack` to reorder a large production table online without holding an `ACCESS EXCLUSIVE` lock.".into(),
                sql: None,
                cost: None,
            });
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec!["Avoid running `CLUSTER` directly on large production tables during traffic.".into()];
            findings.push(f);
        }
        Some("REINDEX") => {
            if upper.iter().any(|t| t == "CONCURRENTLY") {
                return;
            }
            let mut pos = 1;
            if upper.get(pos).map(String::as_str) == Some("(") {
                let mut depth = 0usize;
                for (i, tok) in upper.iter().enumerate().skip(pos) {
                    match tok.as_str() {
                        "(" => depth += 1,
                        ")" => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                pos = i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            let kind = upper.get(pos).map(String::as_str).unwrap_or("");
            let Some(raw_target) = tokens.get(pos + 1) else { return };
            match kind {
                "TABLE" => {
                    let Some(table) = find_table(snapshot, raw_target).filter(|t| is_large_table(t)) else {
                        return;
                    };
                    let rows = table_rows(table);
                    let bytes = table_bytes(table);
                    let qual = table.qualified();
                    let mut f = finding(
                        "sql-lint",
                        &format!("blocking-reindex:{qual}"),
                        Category::Indexes,
                        subject(SubjectKind::Table, &qual),
                    );
                    f.actionability = Actionability::NeedsAttention;
                    f.confidence = Confidence::High;
                    f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
                    f.impact.score = base_score(f.severity);
                    f.impact.label = table_size_label(rows, bytes);
                    f.title = format!(
                        "REINDEX TABLE {} ({}) without CONCURRENTLY blocks reads and writes",
                        table.name,
                        table_size_label(rows, bytes)
                    );
                    f.summary = format!(
                        "The statement `{}` rebuilds all indexes on {} ({}) without `CONCURRENTLY`. Standard `REINDEX \
                         TABLE` holds `SHARE` and `ACCESS EXCLUSIVE` locks that block concurrent writes and reads.",
                        compact_sql(stmt),
                        qual,
                        table_size_label(rows, bytes)
                    );
                    f.fix = Some(Fix {
                        summary: "Use `REINDEX TABLE CONCURRENTLY` (outside a transaction block) so concurrent reads and writes can continue.".into(),
                        sql: (!in_transaction).then(|| format!("REINDEX TABLE CONCURRENTLY {qual};")),
                        cost: Some("REINDEX CONCURRENTLY cannot run inside a transaction block.".into()),
                    });
                    push_table_metrics(&mut f, rows, bytes);
                    f.verify =
                        vec!["Re-run `culprit lint-sql` after switching to `REINDEX TABLE CONCURRENTLY`.".into()];
                    findings.push(f);
                }
                "INDEX" => {
                    let idx_clean = unquote_ident(raw_target);
                    let idx_leaf = idx_clean.rsplit('.').next().unwrap_or(&idx_clean);
                    let Some(ix) = snapshot.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(idx_leaf)) else {
                        return;
                    };
                    let Some(table) = snapshot.table(&ix.schema, &ix.table).filter(|t| is_large_table(t)) else {
                        return;
                    };
                    let rows = table_rows(table);
                    let bytes = table_bytes(table);
                    let qual = table.qualified();
                    let mut f = finding(
                        "sql-lint",
                        &format!("blocking-reindex:{}", ix.name),
                        Category::Indexes,
                        subject(SubjectKind::Index, format!("{qual}.{}", ix.name)),
                    );
                    f.actionability = Actionability::NeedsAttention;
                    f.confidence = Confidence::High;
                    f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
                    f.impact.score = base_score(f.severity);
                    f.impact.label = table_size_label(rows, bytes);
                    f.title = format!(
                        "REINDEX INDEX {} on {} ({}) without CONCURRENTLY blocks writes",
                        ix.name,
                        table.name,
                        table_size_label(rows, bytes)
                    );
                    f.summary = format!(
                        "The statement `{}` rebuilds `{}` on {} ({}) without `CONCURRENTLY`. Standard `REINDEX INDEX` \
                         blocks writes on the parent table and reads of the index until the rebuild finishes.",
                        compact_sql(stmt),
                        ix.name,
                        qual,
                        table_size_label(rows, bytes)
                    );
                    let q_idx = postgres_qualified_index(&ix.schema, &ix.name);
                    f.fix = Some(Fix {
                        summary: "Use `REINDEX INDEX CONCURRENTLY` (outside a transaction block) so concurrent reads and writes can continue.".into(),
                        sql: (!in_transaction).then(|| format!("REINDEX INDEX CONCURRENTLY {q_idx};")),
                        cost: Some("REINDEX CONCURRENTLY cannot run inside a transaction block.".into()),
                    });
                    push_table_metrics(&mut f, rows, bytes);
                    f.verify =
                        vec!["Re-run `culprit lint-sql` after switching to `REINDEX INDEX CONCURRENTLY`.".into()];
                    findings.push(f);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn check_create_index(
    snapshot: &Snapshot,
    stmt: &str,
    tokens: &[String],
    in_transaction: bool,
    findings: &mut Vec<Finding>,
) {
    let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
    if upper.first().map(String::as_str) != Some("CREATE") {
        return;
    }
    let Some(idx_pos) = upper.iter().position(|t| t == "INDEX") else {
        return;
    };
    if idx_pos > 3 {
        return;
    }
    let has_concurrently = upper.iter().any(|t| t == "CONCURRENTLY");
    let Some(on_pos) = upper.iter().position(|t| t == "ON") else {
        return;
    };
    let after_on = if upper.get(on_pos + 1).map(String::as_str) == Some("ONLY") { on_pos + 2 } else { on_pos + 1 };
    let Some(raw_table) = tokens.get(after_on) else {
        return;
    };
    let Some(table) = find_table(snapshot, raw_table) else {
        return;
    };
    if !is_large_table(table) {
        return;
    }
    let engine = snapshot.server.engine;
    let is_blocking = match engine {
        Engine::Postgres => !has_concurrently,
        Engine::Mssql => supports_online_index(snapshot) && !has_mssql_online_on(&upper),
        Engine::Mysql => !has_mysql_online_ddl(&upper),
        _ => false,
    };
    if !is_blocking {
        return;
    }
    let rows = table_rows(table);
    let bytes = table_bytes(table);
    let qual = table.qualified();
    let mut f =
        finding("sql-lint", &format!("blocking-index:{qual}"), Category::Indexes, subject(SubjectKind::Table, &qual));
    f.actionability = Actionability::NeedsAttention;
    f.confidence = Confidence::High;
    f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
    f.impact.score = base_score(f.severity);
    f.impact.label = table_size_label(rows, bytes);
    match engine {
        Engine::Postgres => {
            f.title = format!(
                "CREATE INDEX on {} ({}) without CONCURRENTLY blocks writes until the build finishes",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` builds an index on {} ({}) without `CONCURRENTLY`. Standard `CREATE INDEX` in \
                 PostgreSQL acquires a `SHARE` lock on the table for the entire index build, blocking all `INSERT`, \
                 `UPDATE` and `DELETE` statements.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            f.fix = Some(Fix {
                summary: "Use `CREATE INDEX CONCURRENTLY` (outside a transaction block) so writes continue while the index builds.".into(),
                sql: (!in_transaction).then(|| suggest_concurrent_index(stmt)).flatten(),
                cost: Some("CONCURRENTLY performs two table scans and cannot run inside a transaction block.".into()),
            });
        }
        Engine::Mssql => {
            f.title = format!(
                "CREATE INDEX on {} ({}) without ONLINE = ON blocks writes during the build",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` builds an index on {} ({}) without `WITH (ONLINE = ON)`, which this SQL Server \
                 edition supports. Offline index builds hold shared or schema-modification locks that block writers.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            f.fix = Some(Fix {
                summary:
                    "Add `WITH (ONLINE = ON)` to the `CREATE INDEX` statement so concurrent reads and writes continue."
                        .into(),
                sql: suggest_mssql_online_index(stmt, &upper),
                cost: Some("Online index builds use additional tempdb version store space while running.".into()),
            });
        }
        Engine::Mysql => {
            f.title = format!(
                "CREATE INDEX on {} ({}) without LOCK=NONE can fall back to a table lock",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` builds an index on {} ({}) without `ALGORITHM=INPLACE LOCK=NONE`. Without \
                 explicit `LOCK=NONE`, MySQL/MariaDB can fall back to `LOCK=SHARED` or `ALGORITHM=COPY` if an online \
                 build is not supported, blocking concurrent writes until the build finishes.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            f.fix = Some(Fix {
                summary: "Add `ALGORITHM=INPLACE LOCK=NONE` so the server guarantees an online build and fails immediately instead of locking the table.".into(),
                sql: suggest_mysql_online_index(stmt, &upper),
                cost: Some("Online InnoDB index builds buffer concurrent DML in innodb_online_alter_log_max_size while running.".into()),
            });
        }
        _ => {}
    }
    push_table_metrics(&mut f, rows, bytes);
    f.verify =
        vec!["Re-run `culprit lint-sql` on the updated migration to confirm no blocking index builds remain.".into()];
    findings.push(f);
}

fn collect_validated_constraint_tables(statements: &[String]) -> Vec<String> {
    let mut fk_constraints = Vec::new();
    for stmt in statements {
        let tokens = tokenize(stmt);
        let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
        if upper.windows(2).any(|w| w == ["FOREIGN", "KEY"]) {
            for (i, w) in upper.windows(2).enumerate() {
                if w[0] == "CONSTRAINT" {
                    fk_constraints.push(unquote_ident(&tokens[i + 1]).to_ascii_lowercase());
                }
            }
        }
    }
    let mut tables = Vec::new();
    for stmt in statements {
        let tokens = tokenize(stmt);
        let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
        if upper.first().map(String::as_str) == Some("ALTER")
            && upper.get(1).map(String::as_str) == Some("TABLE")
            && let Some(val_pos) = upper.windows(2).position(|w| w == ["VALIDATE", "CONSTRAINT"])
        {
            let cname = tokens.get(val_pos + 2).map(|c| unquote_ident(c).to_ascii_lowercase()).unwrap_or_default();
            if !cname.is_empty() && fk_constraints.contains(&cname) {
                continue;
            }
            let mut pos = 2;
            if upper.get(pos).map(String::as_str) == Some("IF")
                && upper.get(pos + 1).map(String::as_str) == Some("EXISTS")
            {
                pos += 2;
            }
            if upper.get(pos).map(String::as_str) == Some("ONLY") {
                pos += 1;
            }
            if let Some(raw_table) = tokens.get(pos) {
                tables.push(unquote_ident(raw_table));
            }
        }
    }
    tables
}

fn check_alter_table(
    snapshot: &Snapshot,
    stmt: &str,
    tokens: &[String],
    ctx: &ScriptContext<'_>,
    findings: &mut Vec<Finding>,
) {
    let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
    if upper.first().map(String::as_str) != Some("ALTER") || upper.get(1).map(String::as_str) != Some("TABLE") {
        return;
    }
    let mut pos = 2;
    if upper.get(pos).map(String::as_str) == Some("IF") && upper.get(pos + 1).map(String::as_str) == Some("EXISTS") {
        pos += 2;
    }
    if upper.get(pos).map(String::as_str) == Some("ONLY") {
        pos += 1;
    }
    let Some(raw_table) = tokens.get(pos) else {
        return;
    };
    let Some(table) = find_table(snapshot, raw_table) else {
        return;
    };
    if pos + 1 >= tokens.len() {
        return;
    }

    let before_count = findings.len();
    for (clause_tokens, clause_upper) in split_top_level_commas(&tokens[pos + 1..], &upper[pos + 1..]) {
        check_alter_table_action(snapshot, stmt, table, clause_tokens, clause_upper, ctx, findings);
    }
    let adds_fk = upper.windows(2).any(|w| w == ["FOREIGN", "KEY"]);
    if snapshot.server.engine == Engine::Mysql
        && is_large_table(table)
        && !has_mysql_online_ddl(&upper)
        && !adds_fk
        && !findings[before_count..].iter().any(|f| {
            f.id.starts_with("sql-lint:table-rewrite:")
                || f.id.starts_with("sql-lint:volatile-default:")
                || f.id.starts_with("sql-lint:not-null-without-default:")
        })
    {
        let rows = table_rows(table);
        let bytes = table_bytes(table);
        let qual = table.qualified();
        let mut f = finding(
            "sql-lint",
            &format!("mysql-online-ddl:{qual}"),
            Category::Maintenance,
            subject(SubjectKind::Table, &qual),
        );
        f.actionability = Actionability::NeedsAttention;
        f.confidence = Confidence::High;
        f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
        f.impact.score = base_score(f.severity);
        f.impact.label = table_size_label(rows, bytes);
        f.title = format!(
            "ALTER TABLE on {} ({}) without LOCK=NONE or ALGORITHM=INSTANT can lock the table",
            table.name,
            table_size_label(rows, bytes)
        );
        f.summary = format!(
            "The statement `{}` alters {} ({}) without specifying `ALGORITHM=INSTANT` or `ALGORITHM=INPLACE, \
             LOCK=NONE`. Without an explicit online DDL clause, MySQL/MariaDB can fall back to `ALGORITHM=COPY` or \
             `LOCK=SHARED`, blocking concurrent writes while rebuilding the table.",
            compact_sql(stmt),
            qual,
            table_size_label(rows, bytes)
        );
        let can_append = !upper.iter().any(|t| matches!(t.as_str(), "ALGORITHM" | "LOCK"));
        f.fix = Some(Fix {
            summary: "Append `ALGORITHM=INPLACE, LOCK=NONE` (or `ALGORITHM=INSTANT` for compatible column additions on MySQL 8.0+) so the database rejects any blocking table copy immediately.".into(),
            sql: can_append.then(|| {
                let trimmed = stmt.trim().trim_end_matches(';').trim_end();
                format!("{trimmed}, ALGORITHM=INPLACE, LOCK=NONE;")
            }),
            cost: None,
        });
        push_table_metrics(&mut f, rows, bytes);
        f.verify = vec![
            "Re-run `culprit lint-sql` after adding `ALGORITHM=INPLACE, LOCK=NONE` or `ALGORITHM=INSTANT`.".into(),
        ];
        findings.push(f);
    }
}

fn split_top_level_commas<'a>(tokens: &'a [String], upper: &'a [String]) -> Vec<(&'a [String], &'a [String])> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (idx, tok) in tokens.iter().enumerate() {
        match tok.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                if start < idx {
                    out.push((&tokens[start..idx], &upper[start..idx]));
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        out.push((&tokens[start..], &upper[start..]));
    }
    out
}

fn check_alter_table_action(
    snapshot: &Snapshot,
    stmt: &str,
    table: &TableStat,
    tokens: &[String],
    upper: &[String],
    ctx: &ScriptContext<'_>,
    findings: &mut Vec<Finding>,
) {
    // 1. Check FOREIGN KEY additions for missing child index or column type mismatch.
    if let Some(fk_pos) = upper.windows(2).position(|w| w == ["FOREIGN", "KEY"])
        && let Some((child_cols, rest_idx)) = parse_paren_list(tokens, fk_pos + 2)
        && let Some(ref_offset) = upper[rest_idx..].iter().position(|t| t == "REFERENCES")
    {
        let ref_pos = rest_idx + ref_offset;
        if let Some(raw_parent) = tokens.get(ref_pos + 1) {
            let parent_cols = parse_paren_list(tokens, ref_pos + 2).map(|(cols, _)| cols).unwrap_or_default();
            check_foreign_key_addition(snapshot, stmt, table, &child_cols, raw_parent, &parent_cols, ctx, findings);
        }
    }

    // 1b. Check DROP [COLUMN] <col> or RENAME [COLUMN] <col> against active workload queries.
    if upper.first().map(String::as_str) == Some("DROP") {
        let second = upper.get(1).map(String::as_str).unwrap_or("");
        if !matches!(second, "CONSTRAINT" | "FOREIGN" | "CHECK" | "PRIMARY" | "UNIQUE" | "INDEX") {
            let mut col_idx = if second == "COLUMN" { 2 } else { 1 };
            if upper.get(col_idx).map(String::as_str) == Some("IF")
                && upper.get(col_idx + 1).map(String::as_str) == Some("EXISTS")
            {
                col_idx += 2;
            }
            if let Some(raw_col) = tokens.get(col_idx) {
                let col_name = unquote_ident(raw_col);
                check_workload_references(snapshot, stmt, table, Some(&col_name), "DROP COLUMN", findings);
            }
        }
    } else if upper.first().map(String::as_str) == Some("RENAME") {
        let second = upper.get(1).map(String::as_str).unwrap_or("");
        if second == "COLUMN" {
            if let Some(raw_col) = tokens.get(2) {
                let col_name = unquote_ident(raw_col);
                check_workload_references(snapshot, stmt, table, Some(&col_name), "RENAME COLUMN", findings);
            }
        } else if second == "TO" {
            check_workload_references(snapshot, stmt, table, None, "RENAME TABLE", findings);
        }
    }

    // 1c. Check ADD [COLUMN] <col> <type> NOT NULL without DEFAULT on non-empty tables.
    if upper.first().map(String::as_str) == Some("ADD") && table_rows(table) >= 1.0 {
        let second = upper.get(1).map(String::as_str).unwrap_or("");
        if !matches!(second, "CONSTRAINT" | "FOREIGN" | "CHECK" | "PRIMARY" | "UNIQUE" | "EXCLUDE") {
            let mut col_idx = if second == "COLUMN" { 2 } else { 1 };
            if upper.get(col_idx).map(String::as_str) == Some("IF")
                && upper.get(col_idx + 1).map(String::as_str) == Some("NOT")
                && upper.get(col_idx + 2).map(String::as_str) == Some("EXISTS")
            {
                col_idx += 3;
            }
            if let Some(raw_col) = tokens.get(col_idx) {
                let rest = &upper[col_idx + 1..];
                let has_not_null = rest.windows(2).any(|w| w == ["NOT", "NULL"]);
                let has_default = rest.iter().any(|t| {
                    matches!(
                        t.as_str(),
                        "DEFAULT"
                            | "GENERATED"
                            | "IDENTITY"
                            | "SERIAL"
                            | "BIGSERIAL"
                            | "SMALLSERIAL"
                            | "SERIAL4"
                            | "SERIAL8"
                            | "SERIAL2"
                            | "AUTO_INCREMENT"
                    )
                });
                if has_not_null && !has_default {
                    let rows = table_rows(table);
                    let bytes = table_bytes(table);
                    let qual = table.qualified();
                    let col_name = unquote_ident(raw_col);
                    let mut f = finding(
                        "sql-lint",
                        &format!("not-null-without-default:{qual}.{col_name}"),
                        Category::Maintenance,
                        subject(SubjectKind::Table, &qual),
                    );
                    f.actionability = Actionability::NeedsAttention;
                    f.confidence = Confidence::High;
                    f.severity = Severity::High;
                    f.impact.score = base_score(f.severity);
                    f.impact.label = "fails on existing rows".into();
                    f.title = format!(
                        "ADD COLUMN {}.{} NOT NULL without DEFAULT fails on non-empty table ({})",
                        table.name,
                        col_name,
                        table_size_label(rows, bytes)
                    );
                    f.summary = format!(
                        "The statement `{}` adds `{col_name}` as `NOT NULL` without a `DEFAULT` clause to {} ({}). \
                         Because existing rows would receive `NULL` for `{col_name}`, the database rejects the \
                         statement immediately at runtime.",
                        compact_sql(stmt),
                        qual,
                        table_size_label(rows, bytes)
                    );
                    f.fix = Some(Fix {
                        summary: format!(
                            "Either supply a non-volatile `DEFAULT` value, or add `{col_name}` as nullable first, backfill existing rows, and then enforce `NOT NULL`."
                        ),
                        sql: None,
                        cost: None,
                    });
                    push_table_metrics(&mut f, rows, bytes);
                    f.verify = vec![
                        "Re-run `culprit lint-sql` after adding a `DEFAULT` or making the initial column nullable."
                            .into(),
                    ];
                    findings.push(f);
                }
            }
        }
    }

    // 2. Check table-rewriting or full-scan ALTER TABLE operations on large tables.
    if !is_large_table(table) {
        return;
    }
    let rows = table_rows(table);
    let bytes = table_bytes(table);
    let qual = table.qualified();

    // 2a. Unvalidated FOREIGN KEY or CHECK constraint addition on PostgreSQL without NOT VALID,
    // or UNIQUE / PRIMARY KEY constraint addition without USING INDEX.
    if snapshot.server.engine == Engine::Postgres && upper.first().map(String::as_str) == Some("ADD") {
        let is_fk = upper.windows(2).any(|w| w == ["FOREIGN", "KEY"]);
        let is_check = upper.iter().any(|t| t == "CHECK");
        let has_not_valid = upper.windows(2).any(|w| w == ["NOT", "VALID"]);
        let is_unique = upper.iter().any(|t| t == "UNIQUE");
        let is_pk = upper.windows(2).any(|w| w == ["PRIMARY", "KEY"]);
        let has_using_index = upper.windows(2).any(|w| w == ["USING", "INDEX"]);
        if (is_fk || is_check) && !has_not_valid {
            let kind = if is_fk { "FOREIGN KEY" } else { "CHECK" };
            let cname = if upper.get(1).map(String::as_str) == Some("CONSTRAINT") {
                tokens.get(2).map(|s| unquote_ident(s)).unwrap_or_else(|| kind.to_ascii_lowercase().replace(' ', "-"))
            } else {
                kind.to_ascii_lowercase().replace(' ', "-")
            };
            let mut f = finding(
                "sql-lint",
                &format!("unvalidated-constraint:{qual}:{cname}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "ADD {kind} constraint on {} ({}) without NOT VALID scans the table under a lock",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` adds a {kind} constraint on {} ({}) without `NOT VALID`. PostgreSQL validates \
                 every existing row immediately while holding a `SHARE ROW EXCLUSIVE` / `ACCESS EXCLUSIVE` lock that \
                 blocks writes on {}.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes),
                table.name
            );
            f.fix = Some(Fix {
                summary: format!(
                    "Add the {kind} constraint with `NOT VALID` first, then run `ALTER TABLE {qual} VALIDATE CONSTRAINT {cname};` in a separate step (which only holds `SHARE UPDATE EXCLUSIVE` and allows concurrent reads and writes)."
                ),
                sql: None,
                cost: None,
            });
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec!["Re-run `culprit lint-sql` after adding `NOT VALID` to the constraint definition.".into()];
            findings.push(f);
        } else if (is_unique || is_pk) && !has_using_index {
            let kind = if is_pk { "PRIMARY KEY" } else { "UNIQUE" };
            let cname = if upper.get(1).map(String::as_str) == Some("CONSTRAINT") {
                tokens.get(2).map(|s| unquote_ident(s)).unwrap_or_else(|| kind.to_ascii_lowercase().replace(' ', "-"))
            } else {
                kind.to_ascii_lowercase().replace(' ', "-")
            };
            let mut f = finding(
                "sql-lint",
                &format!("blocking-unique-constraint:{qual}:{cname}"),
                Category::Indexes,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "ADD {kind} constraint on {} ({}) without USING INDEX builds the index under an ACCESS EXCLUSIVE lock",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` adds a {kind} constraint on {} ({}) directly. PostgreSQL builds the backing unique \
                 index while holding an `ACCESS EXCLUSIVE` lock on {}, blocking all reads and writes until the index \
                 build finishes.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes),
                table.name
            );
            f.fix = Some(Fix {
                summary: format!(
                    "Build the unique index first with `CREATE UNIQUE INDEX CONCURRENTLY`, then attach it with `ALTER TABLE {qual} ADD CONSTRAINT {cname} {kind} USING INDEX <index_name>;`."
                ),
                sql: None,
                cost: None,
            });
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec![
                "Re-run `culprit lint-sql` after switching to `CREATE UNIQUE INDEX CONCURRENTLY` + `ADD CONSTRAINT ... USING INDEX`.".into(),
            ];
            findings.push(f);
        }
    }

    // 2b. ADD [COLUMN] <col> <type> ... DEFAULT <volatile_fn>() table rewrite on large tables.
    if upper.first().map(String::as_str) == Some("ADD") {
        let second = upper.get(1).map(String::as_str).unwrap_or("");
        if !matches!(second, "CONSTRAINT" | "FOREIGN" | "CHECK" | "PRIMARY" | "UNIQUE" | "EXCLUDE") {
            let mut col_idx = 1;
            if second == "COLUMN" {
                col_idx += 1;
            }
            if upper.get(col_idx).map(String::as_str) == Some("IF")
                && upper.get(col_idx + 1).map(String::as_str) == Some("NOT")
                && upper.get(col_idx + 2).map(String::as_str) == Some("EXISTS")
            {
                col_idx += 3;
            }
            if let Some(raw_col) = tokens.get(col_idx)
                && let Some(def_pos) = upper[col_idx + 1..].iter().position(|t| t == "DEFAULT")
            {
                let fn_idx = col_idx + 1 + def_pos + 1;
                if let Some(fn_tok) = upper.get(fn_idx)
                    && tokens.get(fn_idx + 1).map(String::as_str) == Some("(")
                    && is_volatile_default_function(fn_tok)
                {
                    let col_name = unquote_ident(raw_col);
                    let fn_display = tokens[fn_idx].clone();
                    let mut f = finding(
                        "sql-lint",
                        &format!("volatile-default:{qual}.{col_name}"),
                        Category::Maintenance,
                        subject(SubjectKind::Table, &qual),
                    );
                    f.actionability = Actionability::NeedsAttention;
                    f.confidence = Confidence::High;
                    f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
                    f.impact.score = base_score(f.severity);
                    f.impact.label = table_size_label(rows, bytes);
                    f.title = format!(
                        "ADD COLUMN {}.{} with volatile DEFAULT {}() rewrites the table ({})",
                        table.name,
                        col_name,
                        fn_display,
                        table_size_label(rows, bytes)
                    );
                    f.summary = format!(
                        "The statement `{}` adds `{col_name}` to {} ({}) with volatile `DEFAULT {fn_display}()`, which \
                         cannot use metadata-only fast defaults because every existing row requires a newly evaluated \
                         value. The engine rewrites every row and index while holding an exclusive table lock.",
                        compact_sql(stmt),
                        qual,
                        table_size_label(rows, bytes)
                    );
                    f.fix = Some(Fix {
                        summary: format!(
                            "Add `{col_name}` without a default, backfill existing rows in batches, and then set `ALTER TABLE {qual} ALTER COLUMN {col_name} SET DEFAULT {fn_display}();`."
                        ),
                        sql: None,
                        cost: None,
                    });
                    push_table_metrics(&mut f, rows, bytes);
                    f.verify =
                        vec!["Verify in staging with `SET lock_timeout = '2s'` before running on production.".into()];
                    findings.push(f);
                }
            }
        }
    }

    // 2c. ALTER [COLUMN] <col> [SET DATA] TYPE <new_type>, MODIFY <col> <new_type>, SQL Server ALTER COLUMN <col> <new_type>, or SET NOT NULL
    if let Some(alter_col_pos) = upper.iter().position(|t| t == "ALTER" || t == "MODIFY") {
        let idx = alter_col_pos;
        let has_set_not_null = upper[idx..].windows(3).any(|w| w == ["SET", "NOT", "NULL"]);
        let table_validated = ctx.validated_tables.iter().any(|raw| table_matches(table, raw));
        let type_change = parse_column_type_change(tokens, upper, idx);
        let is_rewrite = type_change.is_some_and(|(col_name, new_type, has_using_or_collate)| {
            if has_using_or_collate {
                return true;
            }
            let existing = snapshot.columns.iter().find(|c| {
                c.schema.eq_ignore_ascii_case(&table.schema)
                    && c.table.eq_ignore_ascii_case(&table.name)
                    && c.name.eq_ignore_ascii_case(&col_name)
            });
            match existing {
                Some(col) => !is_metadata_only_type_change(snapshot.server.engine, &col.data_type, &new_type),
                None => true,
            }
        });
        if is_rewrite && !findings.iter().any(|f| f.id == format!("sql-lint:table-rewrite:{qual}")) {
            let mut f = finding(
                "sql-lint",
                &format!("table-rewrite:{qual}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = if rows >= HIGH_TABLE_ROWS || bytes >= GB { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "Column type change on {} ({}) can rewrite the table under an exclusive lock",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` alters a column type on {} ({}). Changing a column's representation rewrites \
                 every row and index while holding an `ACCESS EXCLUSIVE` / schema-modification lock, blocking all \
                 reads and writes on {}.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes),
                table.name
            );
            f.fix = Some(Fix {
                summary: "Prefer an expand-and-contract migration (add a new column, backfill in small batches, swap reads/writes) unless the engine guarantees a metadata-only cast.".into(),
                sql: None,
                cost: None,
            });
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec!["Verify in staging with `SET lock_timeout = '2s'` before running on production.".into()];
            findings.push(f);
        } else if has_set_not_null
            && snapshot.server.engine == Engine::Postgres
            && !table_validated
            && !findings.iter().any(|f| f.id == format!("sql-lint:set-not-null:{qual}"))
        {
            let mut f = finding(
                "sql-lint",
                &format!("set-not-null:{qual}"),
                Category::Maintenance,
                subject(SubjectKind::Table, &qual),
            );
            f.actionability = Actionability::NeedsAttention;
            f.confidence = Confidence::High;
            f.severity = Severity::Medium;
            f.impact.score = base_score(f.severity);
            f.impact.label = table_size_label(rows, bytes);
            f.title = format!(
                "SET NOT NULL on {} ({}) scans the table under an ACCESS EXCLUSIVE lock unless backed by a valid constraint",
                table.name,
                table_size_label(rows, bytes)
            );
            f.summary = format!(
                "The statement `{}` runs `SET NOT NULL` on {} ({}). Without an existing validated `CHECK (col IS NOT \
                 NULL)` constraint, PostgreSQL scans the entire table while holding an `ACCESS EXCLUSIVE` lock.",
                compact_sql(stmt),
                qual,
                table_size_label(rows, bytes)
            );
            f.fix = Some(Fix {
                summary: "Add `CHECK (col IS NOT NULL) NOT VALID`, run `ALTER TABLE ... VALIDATE CONSTRAINT ...` (which only takes `SHARE UPDATE EXCLUSIVE`), and then run `SET NOT NULL`.".into(),
                sql: None,
                cost: None,
            });
            push_table_metrics(&mut f, rows, bytes);
            f.verify = vec!["Re-run `culprit lint-sql` after splitting the constraint validation step.".into()];
            findings.push(f);
        }
    }
}

fn is_volatile_default_function(upper_fn: &str) -> bool {
    let bare = upper_fn.rsplit('.').next().unwrap_or(upper_fn);
    matches!(
        bare,
        "GEN_RANDOM_UUID"
            | "UUID_GENERATE_V4"
            | "UUID_GENERATE_V1"
            | "UUID_GENERATE_V1MC"
            | "RANDOM"
            | "CLOCK_TIMESTAMP"
            | "TIMEOFDAY"
            | "NEWID"
            | "NEWSEQUENTIALID"
            | "UUID"
            | "RAND"
    )
}

#[allow(clippy::too_many_arguments)]
fn check_foreign_key_addition(
    snapshot: &Snapshot,
    stmt: &str,
    child_table: &TableStat,
    child_cols: &[String],
    raw_parent: &str,
    parent_cols: &[String],
    ctx: &ScriptContext<'_>,
    findings: &mut Vec<Finding>,
) {
    if child_cols.is_empty() {
        return;
    }
    let child_qual = child_table.qualified();
    let covered_in_snapshot = snapshot.indexes.iter().any(|ix| {
        ix.schema.eq_ignore_ascii_case(&child_table.schema)
            && ix.table.eq_ignore_ascii_case(&child_table.name)
            && !ix.is_disabled
            && index_covers_prefix(&ix.columns, child_cols)
    });
    let covered_in_script = ctx
        .created_indexes
        .iter()
        .any(|(tbl, cols)| table_matches(child_table, tbl) && index_covers_prefix(cols, child_cols));
    if !covered_in_snapshot && !covered_in_script {
        let cols_label = child_cols.join(", ");
        let mut f = finding(
            "sql-lint",
            &format!("unindexed-fk:{child_qual}.{cols_label}"),
            Category::Indexes,
            subject(SubjectKind::Table, &child_qual),
        );
        f.actionability = Actionability::NeedsAttention;
        f.confidence = Confidence::High;
        f.severity = if is_large_table(child_table) { Severity::High } else { Severity::Medium };
        f.impact.score = base_score(f.severity);
        f.impact.label = "unindexed FK".into();
        f.title = format!("New foreign key on {}.({}) has no supporting index", child_table.name, cols_label);
        f.summary = format!(
            "The statement `{}` adds a foreign key on {}.({}) referencing {}, but neither the snapshot nor the migration \
             defines an index with ({}) as its leading key. Deletes or key updates on the referenced parent table \
             will scan {} on every check.",
            compact_sql(stmt),
            child_qual,
            cols_label,
            raw_parent,
            cols_label,
            child_table.name
        );
        let pg_in_tx = ctx.in_transaction && snapshot.server.engine == Engine::Postgres;
        f.fix = Some(Fix {
            summary: format!("Create an index on {child_qual} ({cols_label}) before or alongside the foreign key."),
            sql: (!pg_in_tx).then(|| {
                crate::support::create_index_sql(snapshot, &child_table.schema, &child_table.name, child_cols, &[])
            }),
            cost: None,
        });
        f.verify = vec!["Add the supporting `CREATE INDEX` to the migration and re-run `culprit lint-sql`.".into()];
        findings.push(f);
    }

    if let Some(parent_table) = find_table(snapshot, raw_parent) {
        for (child_col, parent_col) in child_cols.iter().zip(parent_cols.iter()) {
            let c_info = snapshot.columns.iter().find(|c| {
                c.schema.eq_ignore_ascii_case(&child_table.schema)
                    && c.table.eq_ignore_ascii_case(&child_table.name)
                    && c.name.eq_ignore_ascii_case(child_col)
            });
            let p_info = snapshot.columns.iter().find(|c| {
                c.schema.eq_ignore_ascii_case(&parent_table.schema)
                    && c.table.eq_ignore_ascii_case(&parent_table.name)
                    && c.name.eq_ignore_ascii_case(parent_col)
            });
            if let (Some(c), Some(p)) = (c_info, p_info)
                && normalized_base_type(&c.data_type) != normalized_base_type(&p.data_type)
            {
                let mut f = finding(
                    "sql-lint",
                    &format!("fk-type-mismatch:{child_qual}.{}", c.name),
                    Category::Indexes,
                    subject(SubjectKind::Table, &child_qual),
                );
                f.actionability = Actionability::NeedsAttention;
                f.confidence = Confidence::High;
                f.severity = Severity::Medium;
                f.impact.score = base_score(f.severity);
                f.impact.label = format!("{} vs {}", c.data_type, p.data_type);
                f.title = format!(
                    "Foreign key {}.{} ({}) does not match {}.{} ({})",
                    child_table.name, c.name, c.data_type, parent_table.name, p.name, p.data_type
                );
                f.summary = format!(
                    "The statement `{}` references {}.{} (`{}`) from {}.{} (`{}`). Mismatched foreign key types either \
                     fail DDL validation or force implicit conversions during joins and referential integrity checks.",
                    compact_sql(stmt),
                    parent_table.qualified(),
                    p.name,
                    p.data_type,
                    child_qual,
                    c.name,
                    c.data_type
                );
                f.fix = Some(Fix {
                    summary: format!(
                        "Align {}.{}'s type with {}.{} (`{}`).",
                        child_qual,
                        c.name,
                        parent_table.qualified(),
                        p.name,
                        p.data_type
                    ),
                    sql: None,
                    cost: None,
                });
                f.verify = vec!["Confirm both columns share the same data type before adding the constraint.".into()];
                findings.push(f);
            }
        }
    }
}

fn collect_script_indexes(statements: &[String]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for stmt in statements {
        let tokens = tokenize(stmt);
        let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
        if upper.first().map(String::as_str) != Some("CREATE") {
            continue;
        }
        let Some(on_pos) = upper.iter().position(|t| t == "ON") else {
            continue;
        };
        let after_on = if upper.get(on_pos + 1).map(String::as_str) == Some("ONLY") { on_pos + 2 } else { on_pos + 1 };
        let Some(table) = tokens.get(after_on) else {
            continue;
        };
        let mut col_start = after_on + 1;
        if upper.get(col_start).map(String::as_str) == Some("USING") {
            col_start += 2;
        }
        if let Some((cols, _)) = parse_paren_list(&tokens, col_start) {
            out.push((unquote_ident(table), cols));
        }
    }
    out
}

fn index_covers_prefix(index_cols: &[String], fk_cols: &[String]) -> bool {
    if index_cols.len() < fk_cols.len() {
        return false;
    }
    let prefix = &index_cols[..fk_cols.len()];
    fk_cols.iter().all(|fk| prefix.iter().any(|ic| ic.eq_ignore_ascii_case(fk)))
}

fn normalized_base_type(ty: &str) -> String {
    let lower = ty.trim().to_ascii_lowercase();
    let base = lower.split('(').next().unwrap_or(&lower).trim();
    match base {
        "int4" | "serial" | "int" => "integer".into(),
        "int8" | "bigserial" => "bigint".into(),
        "int2" | "smallserial" => "smallint".into(),
        "character varying" => "varchar".into(),
        other => other.into(),
    }
}

fn find_table<'a>(snapshot: &'a Snapshot, raw: &str) -> Option<&'a TableStat> {
    let cleaned = unquote_ident(raw);
    if let Some((schema, name)) = cleaned.rsplit_once('.') {
        snapshot.tables.iter().find(|t| t.schema.eq_ignore_ascii_case(schema) && t.name.eq_ignore_ascii_case(name))
    } else {
        snapshot.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&cleaned))
    }
}

fn table_matches(table: &TableStat, raw: &str) -> bool {
    let cleaned = unquote_ident(raw);
    if let Some((schema, name)) = cleaned.rsplit_once('.') {
        table.schema.eq_ignore_ascii_case(schema) && table.name.eq_ignore_ascii_case(name)
    } else {
        table.name.eq_ignore_ascii_case(&cleaned)
    }
}

fn unquote_ident(raw: &str) -> String {
    raw.split('.')
        .map(|part| part.trim().trim_matches(|c| matches!(c, '"' | '`' | '[' | ']')))
        .collect::<Vec<_>>()
        .join(".")
}

fn is_large_table(t: &TableStat) -> bool {
    table_rows(t) >= LARGE_TABLE_ROWS || table_bytes(t) >= LARGE_TABLE_BYTES
}

fn table_rows(t: &TableStat) -> f64 {
    t.rows.or_else(|| t.storage.as_ref().and_then(|s| s.planner_rows)).unwrap_or(0.0)
}

fn table_bytes(t: &TableStat) -> f64 {
    t.total_bytes.or(t.data_bytes).unwrap_or(0.0)
}

fn table_size_label(rows: f64, bytes: f64) -> String {
    match (rows > 0.0, bytes > 0.0) {
        (true, true) => format!("{} rows, {}", format::count(rows), format::bytes(bytes)),
        (true, false) => format!("{} rows", format::count(rows)),
        (false, true) => format::bytes(bytes),
        (false, false) => "large table".into(),
    }
}

fn push_table_metrics(f: &mut Finding, rows: f64, bytes: f64) {
    if rows > 0.0 {
        f.metrics.push(Metric::new("Rows in snapshot", rows, Unit::Rows));
    }
    if bytes > 0.0 {
        f.metrics.push(Metric::new("Table size in snapshot", bytes, Unit::Bytes));
    }
}

fn has_mssql_online_on(upper: &[String]) -> bool {
    upper.windows(3).any(|w| w == ["ONLINE", "=", "ON"])
}

fn has_mysql_online_ddl(upper: &[String]) -> bool {
    upper.windows(3).any(|w| w == ["LOCK", "=", "NONE"] || w == ["ALGORITHM", "=", "INSTANT"])
        || upper.windows(2).any(|w| w == ["LOCK", "NONE"] || w == ["ALGORITHM", "INSTANT"])
}

fn suggest_mysql_online_index(stmt: &str, upper: &[String]) -> Option<String> {
    if upper.iter().any(|t| matches!(t.as_str(), "ALGORITHM" | "LOCK")) {
        return None;
    }
    let trimmed = stmt.trim().trim_end_matches(';').trim_end();
    Some(format!("{trimmed} ALGORITHM=INPLACE LOCK=NONE;"))
}

fn suggest_concurrent_index(stmt: &str) -> Option<String> {
    let trimmed = stmt.trim().trim_end_matches(';').trim_end();
    let create_pos = find_keyword_outside_comments(trimmed, "CREATE")?;
    let idx_pos = find_keyword_outside_comments(trimmed, "INDEX")?;
    if idx_pos <= create_pos || find_keyword_outside_comments(trimmed, "CONCURRENTLY").is_some() {
        return None;
    }
    let is_unique = find_keyword_outside_comments(&trimmed[create_pos..idx_pos], "UNIQUE").is_some();
    let rest = trimmed[idx_pos + "INDEX".len()..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let head = if is_unique { "CREATE UNIQUE INDEX CONCURRENTLY " } else { "CREATE INDEX CONCURRENTLY " };
    Some(format!("{head}{rest};"))
}

fn suggest_mssql_online_index(stmt: &str, upper: &[String]) -> Option<String> {
    if upper.iter().any(|t| t == "WITH") {
        return None;
    }
    let trimmed = stmt.trim().trim_end_matches(';').trim_end();
    Some(format!("{trimmed} WITH (ONLINE = ON);"))
}

/// Parse an `ALTER [COLUMN] <col> [SET DATA] TYPE <new_type>`, `MODIFY [COLUMN] <col> <new_type>`, or SQL Server
/// `ALTER COLUMN <col> <new_type>` clause into `(column_name, new_type_spec, has_using_or_collate)`.
fn parse_column_type_change(tokens: &[String], upper: &[String], idx: usize) -> Option<(String, String, bool)> {
    let head = upper.get(idx)?.as_str();
    let mut pos = idx + 1;
    let had_column_kw = upper.get(pos).map(String::as_str) == Some("COLUMN");
    if had_column_kw {
        pos += 1;
    }
    let col_name = unquote_ident(tokens.get(pos)?);
    pos += 1;
    let type_start = if head == "MODIFY" {
        pos
    } else if let Some(type_offset) = upper[pos..].iter().position(|t| t == "TYPE") {
        let between = &upper[pos..pos + type_offset];
        if !between.is_empty() && between != ["SET", "DATA"] {
            return None;
        }
        pos + type_offset + 1
    } else if head == "ALTER"
        && had_column_kw
        && let Some(next_kw) = upper.get(pos).map(String::as_str)
        && !matches!(next_kw, "SET" | "DROP" | "RESET" | "ADD" | "TYPE")
    {
        pos
    } else {
        return None;
    };
    if type_start >= tokens.len() {
        return None;
    }
    let has_using_or_collate = upper[type_start..].iter().any(|t| matches!(t.as_str(), "USING" | "COLLATE"));
    let type_end = upper[type_start..]
        .iter()
        .position(|t| {
            matches!(
                t.as_str(),
                "USING"
                    | "COLLATE"
                    | "NULL"
                    | "NOT"
                    | "DEFAULT"
                    | "COMMENT"
                    | "FIRST"
                    | "AFTER"
                    | "GENERATED"
                    | "IDENTITY"
            )
        })
        .map_or(tokens.len(), |off| type_start + off);
    if type_start >= type_end {
        return None;
    }
    let mut new_type = String::new();
    for tok in &tokens[type_start..type_end] {
        if matches!(tok.as_str(), "(" | ")" | ",") {
            new_type.push_str(tok);
        } else {
            if !new_type.is_empty() && !new_type.ends_with(['(', ',']) {
                new_type.push(' ');
            }
            new_type.push_str(tok);
        }
    }
    Some((col_name, new_type, has_using_or_collate))
}

fn parse_type_spec(raw: &str) -> (String, Option<(u32, Option<u32>)>) {
    let lower = raw.trim().to_ascii_lowercase();
    let (base_part, params_part) = match lower.split_once('(') {
        Some((b, p)) => (b, p.split(')').next()),
        None => (lower.as_str(), None),
    };
    let base_collapsed = base_part.split_whitespace().collect::<Vec<_>>().join(" ");
    let base = match base_collapsed.as_str() {
        "character varying" | "varchar" => "varchar".to_string(),
        "national character varying" | "nvarchar" => "nvarchar".to_string(),
        "decimal" | "numeric" => "numeric".to_string(),
        "bit varying" | "varbit" => "varbit".to_string(),
        other => other.to_string(),
    };
    let params = params_part.and_then(|p| {
        let mut parts = p.split(',');
        let p1 = parts.next()?.trim().parse::<u32>().ok()?;
        let p2 = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
        Some((p1, p2))
    });
    (base, params)
}

fn is_metadata_only_type_change(engine: Engine, old_type: &str, new_type: &str) -> bool {
    let (old_base, old_params) = parse_type_spec(old_type);
    let (new_base, new_params) = parse_type_spec(new_type);
    match engine {
        Engine::Postgres => match (old_base.as_str(), new_base.as_str()) {
            ("varchar", "varchar") => match (old_params, new_params) {
                (Some((old_len, None)), Some((new_len, None))) => new_len >= old_len,
                (_, None) => true,
                _ => false,
            },
            ("varchar" | "text", "text") => new_params.is_none(),
            ("numeric", "numeric") => match (old_params, new_params) {
                (Some((old_p, old_s)), Some((new_p, new_s))) => {
                    new_p >= old_p && old_s.unwrap_or(0) == new_s.unwrap_or(0)
                }
                (_, None) => true,
                _ => false,
            },
            ("varbit", "varbit") => match (old_params, new_params) {
                (Some((old_len, None)), Some((new_len, None))) => new_len >= old_len,
                (_, None) => true,
                _ => false,
            },
            _ => false,
        },
        Engine::Mssql => match (old_base.as_str(), new_base.as_str()) {
            ("varchar", "varchar") => matches!(
                (old_params, new_params),
                (Some((old_len, None)), Some((new_len, None))) if new_len >= old_len && new_len <= 8000
            ),
            ("nvarchar", "nvarchar") => matches!(
                (old_params, new_params),
                (Some((old_len, None)), Some((new_len, None))) if new_len >= old_len && new_len <= 4000
            ),
            _ => false,
        },
        _ => false,
    }
}

fn postgres_qualified_index(schema: &str, name: &str) -> String {
    let d = Engine::Postgres.dialect();
    let needs_quote = |s: &str| {
        s.is_empty()
            || !s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            || !s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    let q_name = if needs_quote(name) { d.quote(name) } else { name.to_string() };
    if schema.is_empty() || schema.eq_ignore_ascii_case("public") {
        q_name
    } else {
        let q_schema = if needs_quote(schema) { d.quote(schema) } else { schema.to_string() };
        format!("{q_schema}.{q_name}")
    }
}

fn rewrite_preserving_comments(orig_span: &str, rep_sql: &str) -> String {
    // If orig_span has no comments, use rep_sql directly.
    if !orig_span.contains("--") && !orig_span.contains("/*") {
        return rep_sql.to_string();
    }
    let rep_upper = rep_sql.to_ascii_uppercase();
    // 1. CREATE [UNIQUE] INDEX -> insert CONCURRENTLY right after INDEX in orig_span
    if rep_upper.starts_with("CREATE ")
        && rep_upper.contains(" INDEX CONCURRENTLY ")
        && let Some(pos) = find_keyword_outside_comments(orig_span, "INDEX")
    {
        let after_idx = pos + "INDEX".len();
        let mut rewritten = String::with_capacity(orig_span.len() + 16);
        rewritten.push_str(&orig_span[..after_idx]);
        rewritten.push_str(" CONCURRENTLY");
        rewritten.push_str(&orig_span[after_idx..]);
        if !rewritten.trim_end().ends_with(';') && rep_sql.ends_with(';') {
            rewritten.push(';');
        }
        return rewritten;
    }
    // 2. Suffix additions (WITH (ONLINE = ON); or ALGORITHM=INPLACE[ ,] LOCK=NONE;)
    for suffix in [" WITH (ONLINE = ON);", " ALGORITHM=INPLACE LOCK=NONE;", ", ALGORITHM=INPLACE, LOCK=NONE;"] {
        if rep_sql.ends_with(suffix) {
            let body = orig_span.trim_end().strip_suffix(';').unwrap_or(orig_span.trim_end());
            return format!("{body}{suffix}");
        }
    }
    // 3. Preserve leading comments before a rewritten statement (DROP INDEX CONCURRENTLY, REINDEX CONCURRENTLY, VACUUM)
    if let Some(first_kw) =
        ["DROP", "REINDEX", "VACUUM"].into_iter().find_map(|kw| find_keyword_outside_comments(orig_span, kw))
        && first_kw > 0
    {
        return format!("{}{rep_sql}", &orig_span[..first_kw]);
    }
    rep_sql.to_string()
}

fn find_keyword_outside_comments(sql: &str, keyword: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let kw = keyword.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        if !in_single && !in_double {
            if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            if i + kw.len() <= bytes.len() && bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
                let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
                let after_ok = i + kw.len() == bytes.len()
                    || !bytes[i + kw.len()].is_ascii_alphanumeric() && bytes[i + kw.len()] != b'_';
                if before_ok && after_ok {
                    return Some(i);
                }
            }
        }
        if bytes[i] == b'\'' && !in_double {
            in_single = !in_single;
        } else if bytes[i] == b'"' && !in_single {
            in_double = !in_double;
        }
        i += 1;
    }
    None
}

fn compact_sql(stmt: &str) -> String {
    stmt.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_statements(sql: &str) -> Vec<String> {
    split_statement_spans(sql).into_iter().map(|(_, _, s)| s).collect()
}

fn parse_dollar_tag(sql: &str) -> Option<&str> {
    let rest = sql.strip_prefix('$')?;
    let end = rest.find('$')?;
    let tag = &rest[..end];
    if tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { Some(&sql[..end + 2]) } else { None }
}

fn split_statement_spans(sql: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut stmt_start: Option<usize> = None;
    let mut stmt_end: usize = 0;
    let mut chars = sql.char_indices().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut dollar_tag: Option<String> = None;
    while let Some((idx, c)) = chars.next() {
        if let Some(ref tag) = dollar_tag {
            current.push(c);
            stmt_end = idx + c.len_utf8();
            if c == '$' && sql[idx..].starts_with(tag.as_str()) {
                for _ in 0..tag.chars().count().saturating_sub(1) {
                    if let Some((end_i, ch)) = chars.next() {
                        current.push(ch);
                        stmt_end = end_i + ch.len_utf8();
                    }
                }
                dollar_tag = None;
            }
            continue;
        }
        if (in_single || in_double) && c == '\\' {
            current.push(c);
            if let Some((end_i, next_c)) = chars.next() {
                current.push(next_c);
                stmt_end = end_i + next_c.len_utf8();
            }
            continue;
        }
        if !in_single && !in_double {
            if c == '$'
                && let Some(tag) = parse_dollar_tag(&sql[idx..])
            {
                if stmt_start.is_none() {
                    stmt_start = Some(idx);
                }
                current.push(c);
                stmt_end = idx + c.len_utf8();
                for _ in 0..tag.chars().count().saturating_sub(1) {
                    if let Some((end_i, ch)) = chars.next() {
                        current.push(ch);
                        stmt_end = end_i + ch.len_utf8();
                    }
                }
                dollar_tag = Some(tag.to_string());
                continue;
            }
            if c == '-' && chars.peek().map(|(_, ch)| *ch) == Some('-') {
                for (_, ch) in chars.by_ref() {
                    if ch == '\n' {
                        current.push('\n');
                        break;
                    }
                }
                continue;
            }
            if c == '/' && chars.peek().map(|(_, ch)| *ch) == Some('*') {
                chars.next();
                while let Some((_, ch)) = chars.next() {
                    if ch == '*' && chars.peek().map(|(_, next_ch)| *next_ch) == Some('/') {
                        chars.next();
                        break;
                    }
                }
                current.push(' ');
                continue;
            }
            if c == ';' {
                let trimmed = current.trim();
                if !trimmed.is_empty()
                    && let Some(start) = stmt_start.take()
                {
                    out.push((start, idx + 1, trimmed.to_string()));
                }
                current.clear();
                stmt_start = None;
                continue;
            }
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        }
        if !c.is_whitespace() {
            if stmt_start.is_none() {
                stmt_start = Some(idx);
            }
            stmt_end = idx + c.len_utf8();
        }
        current.push(c);
    }
    let trimmed = current.trim();
    if !trimmed.is_empty()
        && let Some(start) = stmt_start.take()
    {
        out.push((start, stmt_end, trimmed.to_string()));
    }
    out
}

fn tokenize(stmt: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut close_delim: Option<char> = None;
    for c in stmt.chars() {
        if let Some(q) = close_delim {
            current.push(c);
            if c == q {
                close_delim = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            close_delim = Some(c);
            current.push(c);
            continue;
        }
        if c == '[' {
            close_delim = Some(']');
            current.push(c);
            continue;
        }
        if c.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else if matches!(c, '(' | ')' | ',' | '=') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(c.to_string());
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_paren_list(tokens: &[String], start: usize) -> Option<(Vec<String>, usize)> {
    if tokens.get(start).map(String::as_str) != Some("(") {
        return None;
    }
    let mut cols = Vec::new();
    let mut current: Option<String> = None;
    let mut depth = 0usize;
    for (idx, tok) in tokens.iter().enumerate().skip(start) {
        match tok.as_str() {
            "(" => {
                depth += 1;
            }
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(col) = current.take() {
                        cols.push(col);
                    }
                    return Some((cols, idx + 1));
                }
            }
            "," if depth == 1 => {
                if let Some(col) = current.take() {
                    cols.push(col);
                }
            }
            other if depth == 1 && current.is_none() => {
                let u = other.to_ascii_uppercase();
                if !matches!(u.as_str(), "ASC" | "DESC" | "NULLS" | "FIRST" | "LAST") {
                    current = Some(unquote_ident(other));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postgres_snapshot() -> Snapshot {
        serde_json::from_str(include_str!("../../../../fixtures/postgres-demo.json")).unwrap()
    }

    #[test]
    fn flags_blocking_create_index_unless_concurrent() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let sql = format!("CREATE INDEX idx_test ON {table} (created_at);");
        let out = lint_sql(&s, &sql);
        assert_eq!(out.len(), 1);
        assert!(out[0].id.starts_with("sql-lint:blocking-index:"));
        assert!(out[0].fix.as_ref().unwrap().sql.as_deref().unwrap().contains("CREATE INDEX CONCURRENTLY"));

        let ok_sql = format!("CREATE INDEX CONCURRENTLY idx_test ON {table} (created_at);");
        assert!(lint_sql(&s, &ok_sql).is_empty());
    }

    #[test]
    fn flags_unindexed_and_mismatched_foreign_key_additions() {
        let mut s = postgres_snapshot();
        let child = s.tables[0].name.clone();
        let parent = s.tables[1].name.clone();
        let schema = s.tables[0].schema.clone();
        s.columns.push(culprit_core::ColumnInfo {
            schema: schema.clone(),
            table: child.clone(),
            name: "parent_ref_id".into(),
            data_type: "integer".into(),
            nullable: true,
            default: None,
            generated: false,
            ordinal: 99,
        });
        s.columns.retain(|c| !(c.table == parent && c.name == "id"));
        s.columns.push(culprit_core::ColumnInfo {
            schema: schema.clone(),
            table: parent.clone(),
            name: "id".into(),
            data_type: "bigint".into(),
            nullable: false,
            default: None,
            generated: false,
            ordinal: 1,
        });
        let sql = format!(
            "ALTER TABLE {child} ADD CONSTRAINT fk_parent FOREIGN KEY (parent_ref_id) REFERENCES {parent} (id);"
        );
        let out = lint_sql(&s, &sql);
        assert!(out.iter().any(|f| f.id.starts_with("sql-lint:unindexed-fk:")));
        assert!(out.iter().any(|f| f.id.starts_with("sql-lint:fk-type-mismatch:")));

        // Including a CREATE INDEX in the same migration clears the unindexed-fk warning.
        let with_index = format!("CREATE INDEX CONCURRENTLY idx_child_parent ON {child} (parent_ref_id);\n{sql}");
        let out2 = lint_sql(&s, &with_index);
        assert!(!out2.iter().any(|f| f.id.starts_with("sql-lint:unindexed-fk:")));
        assert!(out2.iter().any(|f| f.id.starts_with("sql-lint:fk-type-mismatch:")));
    }

    #[test]
    fn flags_table_rewrite_and_set_not_null_on_large_tables() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(1_200_000.0);
        let table = s.tables[0].name.clone();
        let sql = format!(
            "ALTER TABLE {table} ALTER COLUMN payload TYPE text;\nALTER TABLE {table} ALTER COLUMN payload SET NOT NULL;"
        );
        let out = lint_sql(&s, &sql);
        assert!(out.iter().any(|f| f.id.starts_with("sql-lint:table-rewrite:")));
        assert!(out.iter().any(|f| f.id.starts_with("sql-lint:set-not-null:")));
    }

    #[test]
    fn flags_concurrent_index_in_transaction_and_multi_action_alter_table() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let sql = format!(
            "BEGIN;\nCREATE INDEX CONCURRENTLY idx_tx ON {table} (created_at);\nCOMMIT;\n\
             ALTER TABLE {table} ADD COLUMN token uuid DEFAULT gen_random_uuid(), \
             ADD COLUMN created_ts timestamptz DEFAULT now(), \
             ADD CONSTRAINT chk_positive CHECK (amount > 0), \
             ALTER COLUMN status SET NOT NULL;"
        );
        let out = lint_sql(&s, &sql);
        assert!(out.iter().any(|f| f.id == "sql-lint:concurrent-in-tx:idx_tx"), "{out:#?}");
        assert!(out.iter().any(|f| f.id.ends_with(".token") && f.id.starts_with("sql-lint:volatile-default:")));
        assert!(!out.iter().any(|f| f.id.ends_with(".created_ts")));
        assert!(
            out.iter().any(|f| f.id.starts_with("sql-lint:unvalidated-constraint:") && f.id.ends_with(":chk_positive"))
        );
        assert!(out.iter().any(|f| f.id.starts_with("sql-lint:set-not-null:")));

        // Adding NOT VALID to the CHECK constraint suppresses the unvalidated-constraint warning.
        let valid_sql = format!("ALTER TABLE {table} ADD CONSTRAINT chk_positive CHECK (amount > 0) NOT VALID;");
        assert!(lint_sql(&s, &valid_sql).is_empty());
    }

    #[test]
    fn flags_blocking_drop_index_truncate_and_workload_breaking_ddl() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let schema = s.tables[0].schema.clone();
        let idx_name =
            s.indexes.iter().find(|i| i.schema == schema && i.table == table).map(|i| i.name.clone()).unwrap();
        if let Some(w) = s.workload.as_mut()
            && let Some(q) = w.queries.first_mut()
        {
            q.text = format!("SELECT id, legacy_col FROM {schema}.{table} WHERE id = $1");
            q.plan = None;
            q.last_seen = None;
        }
        let sql = format!(
            "DROP INDEX {idx_name};\n\
             TRUNCATE TABLE {table};\n\
             ALTER TABLE {table} DROP COLUMN legacy_col;"
        );
        let out = lint_sql(&s, &sql);
        assert!(out.iter().any(|f| f.id == format!("sql-lint:blocking-drop-index:{idx_name}")), "{out:#?}");
        assert!(out.iter().any(|f| f.id == format!("sql-lint:truncate:{schema}.{table}")));
        assert!(out.iter().any(|f| f.id == format!("sql-lint:workload-break:{schema}.{table}.legacy_col")));
    }

    #[test]
    fn flags_not_null_without_default_blocking_unique_and_recognizes_validated_not_null() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let schema = s.tables[0].schema.clone();
        let sql = format!(
            "ALTER TABLE {table} ADD COLUMN required_code text NOT NULL;\n\
             ALTER TABLE {table} ADD CONSTRAINT uq_code UNIQUE (required_code);"
        );
        let out = lint_sql(&s, &sql);
        assert!(
            out.iter().any(|f| f.id == format!("sql-lint:not-null-without-default:{schema}.{table}.required_code"))
        );
        assert!(out.iter().any(|f| f.id == format!("sql-lint:blocking-unique-constraint:{schema}.{table}:uq_code")));

        let safe_sql = format!(
            "ALTER TABLE {table} ADD CONSTRAINT chk_status_nn CHECK (status IS NOT NULL) NOT VALID;\n\
             ALTER TABLE {table} VALIDATE CONSTRAINT chk_status_nn;\n\
             ALTER TABLE {table} ALTER COLUMN status SET NOT NULL;\n\
             CREATE UNIQUE INDEX CONCURRENTLY idx_uq_code ON {table} (required_code);\n\
             ALTER TABLE {table} ADD CONSTRAINT uq_code UNIQUE USING INDEX idx_uq_code;"
        );
        assert!(lint_sql(&s, &safe_sql).is_empty());
    }

    #[test]
    fn skips_metadata_only_type_widenings_and_flags_rewriting_type_changes() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let schema = s.tables[0].schema.clone();
        s.columns.push(culprit_core::ColumnInfo {
            schema: schema.clone(),
            table: table.clone(),
            name: "short_code".into(),
            data_type: "character varying(50)".into(),
            nullable: true,
            default: None,
            generated: false,
            ordinal: 90,
        });
        s.columns.push(culprit_core::ColumnInfo {
            schema: schema.clone(),
            table: table.clone(),
            name: "amount".into(),
            data_type: "numeric(10,2)".into(),
            nullable: true,
            default: None,
            generated: false,
            ordinal: 91,
        });

        // Metadata-only widenings in PostgreSQL do not rewrite the table.
        let widen_sql = format!(
            "ALTER TABLE {table} ALTER COLUMN short_code TYPE varchar(120);\n\
             ALTER TABLE {table} ALTER COLUMN short_code TYPE text;\n\
             ALTER TABLE {table} ALTER COLUMN amount TYPE numeric(14, 2);\n\
             ALTER TABLE {table} ALTER COLUMN amount TYPE numeric;"
        );
        assert!(lint_sql(&s, &widen_sql).is_empty());

        // Narrowing length, changing scale, or supplying a USING clause rewrites the table.
        let narrow_sql = format!("ALTER TABLE {table} ALTER COLUMN short_code TYPE varchar(20);");
        assert!(lint_sql(&s, &narrow_sql).iter().any(|f| f.id.starts_with("sql-lint:table-rewrite:")));

        let scale_sql = format!("ALTER TABLE {table} ALTER COLUMN amount TYPE numeric(14, 4);");
        assert!(lint_sql(&s, &scale_sql).iter().any(|f| f.id.starts_with("sql-lint:table-rewrite:")));

        let using_sql = format!("ALTER TABLE {table} ALTER COLUMN short_code TYPE text USING trim(short_code);");
        assert!(lint_sql(&s, &using_sql).iter().any(|f| f.id.starts_with("sql-lint:table-rewrite:")));
    }

    #[test]
    fn flags_vacuum_full_cluster_blocking_reindex_and_suppresses_concurrent_quick_fix_in_tx() {
        let mut s = postgres_snapshot();
        s.tables[0].rows = Some(500_000.0);
        let table = s.tables[0].name.clone();
        let schema = s.tables[0].schema.clone();
        let idx_name =
            s.indexes.iter().find(|i| i.schema == schema && i.table == table).map(|i| i.name.clone()).unwrap();

        let maint_sql = format!(
            "VACUUM FULL {table};\n\
             CLUSTER {table} USING {idx_name};\n\
             REINDEX TABLE {table};\n\
             REINDEX INDEX {idx_name};"
        );
        let out = lint_sql(&s, &maint_sql);
        assert!(out.iter().any(|f| f.id == format!("sql-lint:vacuum-full:{schema}.{table}")), "{out:#?}");
        assert!(out.iter().any(|f| f.id == format!("sql-lint:cluster:{schema}.{table}")));
        assert!(out.iter().any(|f| f.id == format!("sql-lint:blocking-reindex:{schema}.{table}")));
        assert!(out.iter().any(|f| f.id == format!("sql-lint:blocking-reindex:{idx_name}")));

        // Inside BEGIN ... COMMIT, CREATE INDEX omits the CONCURRENTLY quick-fix SQL.
        let tx_sql = format!("BEGIN;\ncreate  unique\n  index idx_new on {table} (id);\nCOMMIT;");
        let tx_out = lint_sql(&s, &tx_sql);
        assert_eq!(tx_out.len(), 1);
        assert!(tx_out[0].fix.as_ref().unwrap().sql.is_none());

        // Outside a transaction, mixed-case / multiline CREATE UNIQUE INDEX gets a clean CONCURRENTLY quick fix.
        let non_tx_sql = format!("create  unique\n  index idx_new on {table} (id);");
        let non_tx_out = lint_sql(&s, &non_tx_sql);
        assert_eq!(
            non_tx_out[0].fix.as_ref().unwrap().sql.as_deref(),
            Some(format!("CREATE UNIQUE INDEX CONCURRENTLY idx_new on {table} (id);").as_str())
        );

        // MySQL online DDL: CREATE INDEX, DROP INDEX, and ALTER TABLE without LOCK=NONE or ALGORITHM=INSTANT are flagged.
        let mut mysql_s = s.clone();
        mysql_s.server.engine = Engine::Mysql;
        let mysql_sql = format!(
            "CREATE INDEX idx_mysql_test ON {table} (created_at);\n\
             DROP INDEX {idx_name} ON {table};\n\
             ALTER TABLE {table} ADD COLUMN notes varchar(120);"
        );
        let mysql_out = lint_sql(&mysql_s, &mysql_sql);
        assert!(
            mysql_out.iter().any(|f| f.id == format!("sql-lint:blocking-index:{schema}.{table}")),
            "{mysql_out:#?}"
        );
        assert!(mysql_out.iter().any(|f| f.id == format!("sql-lint:blocking-drop-index:{idx_name}")));
        assert!(mysql_out.iter().any(|f| f.id == format!("sql-lint:mysql-online-ddl:{schema}.{table}")));

        let mysql_safe = format!(
            "CREATE INDEX idx_mysql_test ON {table} (created_at) ALGORITHM=INPLACE LOCK=NONE;\n\
             DROP INDEX {idx_name} ON {table} ALGORITHM=INPLACE LOCK=NONE;\n\
             ALTER TABLE {table} ADD COLUMN notes varchar(120), ALGORITHM=INSTANT;"
        );
        assert!(lint_sql(&mysql_s, &mysql_safe).is_empty());

        // Auto-fixing rewrites fixable statements in place and resolves their lint findings.
        let raw_migration = format!(
            "-- migration header\n\
             CREATE INDEX /* keep comment */ idx_auto_fix ON {table} (created_at);\n\
             DROP INDEX {idx_name};\n"
        );
        let (rewritten, count) = apply_sql_fixes(&s, &raw_migration);
        assert_eq!(count, 2);
        assert!(rewritten.contains("-- migration header"));
        assert!(rewritten.contains("CREATE INDEX CONCURRENTLY /* keep comment */ idx_auto_fix"));
        assert!(rewritten.contains(&format!("DROP INDEX CONCURRENTLY IF EXISTS {idx_name};")));
        assert!(lint_sql(&s, &rewritten).is_empty(), "{:?}", lint_sql(&s, &rewritten));

        // Dollar-quoted PL/pgSQL function bodies containing BEGIN ...; END; do not corrupt transaction state.
        let plpgsql_sql = format!(
            "CREATE OR REPLACE FUNCTION touch_row() RETURNS trigger AS $$\n\
             BEGIN\n\
               NEW.updated_at = now();\n\
               RETURN NEW;\n\
             END;\n\
             $$ LANGUAGE plpgsql;\n\
             CREATE INDEX CONCURRENTLY idx_after_fn ON {table} (created_at);\n"
        );
        assert!(lint_sql(&s, &plpgsql_sql).is_empty());

        // Validating a foreign key constraint does not suppress SET NOT NULL on the same table.
        let fk_val_sql = format!(
            "ALTER TABLE {table} ADD CONSTRAINT fk_ref FOREIGN KEY (id) REFERENCES {table} (id) NOT VALID;\n\
             ALTER TABLE {table} VALIDATE CONSTRAINT fk_ref;\n\
             ALTER TABLE {table} ALTER COLUMN status SET NOT NULL;"
        );
        assert!(lint_sql(&s, &fk_val_sql).iter().any(|f| f.id.starts_with("sql-lint:set-not-null:")));
    }
}
