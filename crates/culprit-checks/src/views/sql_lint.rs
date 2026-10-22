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

/// Lint one or more SQL statements (such as a migration file) against the catalog and table sizes in `snapshot`.
pub fn lint_sql(snapshot: &Snapshot, sql: &str) -> Vec<Finding> {
    let statements = split_statements(sql);
    let created_indexes = collect_script_indexes(&statements);
    let mut findings = Vec::new();
    let mut in_transaction = false;
    for stmt in &statements {
        let tokens = tokenize(stmt);
        if tokens.is_empty() {
            continue;
        }
        let upper: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
        if is_transaction_begin(&upper) {
            in_transaction = true;
            continue;
        }
        if is_transaction_end(&upper) {
            in_transaction = false;
            continue;
        }
        check_concurrent_in_tx(snapshot, stmt, &tokens, &upper, in_transaction, &mut findings);
        check_create_index(snapshot, stmt, &tokens, &mut findings);
        check_alter_table(snapshot, stmt, &tokens, &created_indexes, &mut findings);
    }
    findings
}

fn is_transaction_begin(upper: &[String]) -> bool {
    matches!(upper.first().map(String::as_str), Some("BEGIN"))
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
    f.title = format!("{head} INDEX CONCURRENTLY ({target}) cannot run inside a transaction block");
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

fn check_create_index(snapshot: &Snapshot, stmt: &str, tokens: &[String], findings: &mut Vec<Finding>) {
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
                sql: Some(suggest_concurrent_index(stmt)),
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
                sql: None,
                cost: Some("Online index builds use additional tempdb version store space while running.".into()),
            });
        }
        _ => {}
    }
    push_table_metrics(&mut f, rows, bytes);
    f.verify =
        vec!["Re-run `culprit lint-sql` on the updated migration to confirm no blocking index builds remain.".into()];
    findings.push(f);
}

fn check_alter_table(
    snapshot: &Snapshot,
    stmt: &str,
    tokens: &[String],
    created_indexes: &[(String, Vec<String>)],
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

    for (clause_tokens, clause_upper) in split_top_level_commas(&tokens[pos + 1..], &upper[pos + 1..]) {
        check_alter_table_action(snapshot, stmt, table, clause_tokens, clause_upper, created_indexes, findings);
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
    created_indexes: &[(String, Vec<String>)],
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
            check_foreign_key_addition(
                snapshot,
                table,
                &child_cols,
                raw_parent,
                &parent_cols,
                created_indexes,
                findings,
            );
        }
    }

    // 2. Check table-rewriting or full-scan ALTER TABLE operations on large tables.
    if !is_large_table(table) {
        return;
    }
    let rows = table_rows(table);
    let bytes = table_bytes(table);
    let qual = table.qualified();

    // 2a. Unvalidated FOREIGN KEY or CHECK constraint addition on PostgreSQL without NOT VALID.
    if snapshot.server.engine == Engine::Postgres && upper.first().map(String::as_str) == Some("ADD") {
        let is_fk = upper.windows(2).any(|w| w == ["FOREIGN", "KEY"]);
        let is_check = upper.iter().any(|t| t == "CHECK");
        let has_not_valid = upper.windows(2).any(|w| w == ["NOT", "VALID"]);
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
                        "Adding `{col_name}` to {} ({}) with volatile `DEFAULT {fn_display}()` cannot use metadata-only \
                         fast defaults because every existing row requires a newly evaluated value. The engine rewrites \
                         every row and index while holding an exclusive table lock.",
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

    // 2c. ALTER [COLUMN] <col> [SET DATA] TYPE <new_type> or SET NOT NULL
    if let Some(alter_col_pos) = upper.iter().position(|t| t == "ALTER" || t == "MODIFY") {
        let idx = alter_col_pos;
        let has_type = upper[idx..].iter().any(|t| t == "TYPE") || upper.get(idx).map(String::as_str) == Some("MODIFY");
        let has_set_not_null = upper[idx..].windows(3).any(|w| w == ["SET", "NOT", "NULL"]);
        if has_type && !findings.iter().any(|f| f.id == format!("sql-lint:table-rewrite:{qual}")) {
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

fn check_foreign_key_addition(
    snapshot: &Snapshot,
    child_table: &TableStat,
    child_cols: &[String],
    raw_parent: &str,
    parent_cols: &[String],
    created_indexes: &[(String, Vec<String>)],
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
    let covered_in_script = created_indexes
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
            "The migration adds a foreign key on {}.({}) referencing {}, but neither the snapshot nor the migration \
             defines an index with ({}) as its leading key. Deletes or key updates on the referenced parent table \
             will scan {} on every check.",
            child_qual, cols_label, raw_parent, cols_label, child_table.name
        );
        f.fix = Some(Fix {
            summary: format!("Create an index on {child_qual} ({cols_label}) before or alongside the foreign key."),
            sql: Some(crate::support::create_index_sql(
                snapshot,
                &child_table.schema,
                &child_table.name,
                child_cols,
                &[],
            )),
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
                    "{}.{} is `{}` while the referenced column {}.{} is `{}`. Mismatched foreign key types either \
                     fail DDL validation or force implicit conversions during joins and referential integrity checks.",
                    child_qual,
                    c.name,
                    c.data_type,
                    parent_table.qualified(),
                    p.name,
                    p.data_type
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

fn suggest_concurrent_index(stmt: &str) -> String {
    let trimmed = stmt.trim();
    for prefix in ["CREATE UNIQUE INDEX ", "create unique index ", "CREATE INDEX ", "create index "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let head = if prefix.to_ascii_uppercase().contains("UNIQUE") {
                "CREATE UNIQUE INDEX CONCURRENTLY "
            } else {
                "CREATE INDEX CONCURRENTLY "
            };
            return format!("{head}{rest};");
        }
    }
    trimmed.to_string()
}

fn compact_sql(stmt: &str) -> String {
    stmt.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        if !in_single && !in_double {
            if c == '-' && chars.peek() == Some(&'-') {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        current.push('\n');
                        break;
                    }
                }
                continue;
            }
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                while let Some(ch) = chars.next() {
                    if ch == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
                current.push(' ');
                continue;
            }
            if c == ';' {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                current.clear();
                continue;
            }
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        }
        current.push(c);
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

fn tokenize(stmt: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = None;
    for c in stmt.chars() {
        if let Some(q) = in_quotes {
            current.push(c);
            if c == q {
                in_quotes = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            in_quotes = Some(c);
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
}
