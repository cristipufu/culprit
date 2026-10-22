use crate::Engine;
use crate::sql::safety::MSSQL_STATEMENTS;
use crate::sql::*;

#[test]
fn foreign_key_coverage_cannot_come_from_a_filtered_or_mixed_script() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
        for sql in [
            "CREATE INDEX ix_orders ON shop.orders (state)",
            "/* WHERE state = 1 */ CREATE INDEX ix_orders ON shop.orders (state)",
        ] {
            assert!(creates_unfiltered_index(sql, engine), "{engine:?}: {sql}");
        }
        for sql in [
            "",
            "-- CREATE INDEX ix_orders ON shop.orders (state)",
            "CREATE INDEX ix_orders ON shop.orders (state)\nWHERE state = 1",
            "CREATE INDEX ix_orders ON shop.orders (state) /* filter */ WHERE state IS NOT NULL",
            "CREATE INDEX ix_orders ON shop.orders (state); CREATE INDEX ix_other ON shop.orders (region) WHERE region IS NOT NULL",
            "CREATE INDEX ix_orders ON shop.orders (state); SELECT 1",
            "CREATE INDEX ix_orders ON shop.orders (state); CREATE TABLE other (id int)",
        ] {
            assert!(!creates_unfiltered_index(sql, engine), "{engine:?}: {sql}");
        }
    }
    assert!(creates_unfiltered_index("CREATE INDEX [where] ON [shop].[orders] ([where])", Engine::Mssql));
    assert!(creates_unfiltered_index("CREATE INDEX \"where\" ON shop.orders (\"where\")", Engine::Postgres));
    assert!(creates_unfiltered_index("CREATE INDEX `where` ON shop.orders (`where`)", Engine::Mysql));
}

#[test]
fn literal_offsets_do_not_accept_expressions_identifiers_or_nested_queries() {
    for engine in Engine::ALL {
        assert_eq!(offset_count("SELECT * FROM t OFFSET 12000 ROWS FETCH NEXT 10 ROWS ONLY", engine), Some(12000.0));
        for sql in [
            "SELECT 'OFFSET 12000' FROM t",
            "SELECT * FROM (SELECT * FROM t OFFSET 12000) q",
            "SELECT * FROM t OFFSET 12000 + 1",
            "SELECT * FROM t OFFSET :offset",
            "SELECT * FROM t /* OFFSET 12000 */",
            "SELECT * FROM t OFFSET 12000.5",
        ] {
            assert_eq!(offset_count(sql, engine), None, "{engine}: {sql}");
        }
    }
    assert_eq!(offset_count("SELECT * FROM t LIMIT 12000,20", Engine::Mysql), Some(12000.0));
    assert_eq!(offset_count("SELECT * FROM t LIMIT 12000,20", Engine::Sqlite), Some(12000.0));
}

#[test]
fn statistics_maintenance_is_code_not_a_table_name_or_comment() {
    for (text, table) in [
        ("UPDATE STATISTICS dbo.Orders WITH FULLSCAN", "Orders"),
        ("/* before */ UPDATE /* nested /* why */ */ STATISTICS [dbo].[Order Lines]", "Order Lines"),
        ("(@P1 int) UPDATE STATISTICS dbo.Orders", "Orders"),
        ("UPDATE STATISTICS [dbo].[set] WITH FULLSCAN", "set"),
        ("UPDATE STATISTICS [dbo].[Order]]Lines]", "Order]Lines"),
    ] {
        assert_eq!(mssql_statistics_target(text).as_deref(), Some(table), "{text}");
    }
    for text in [
        "-- UPDATE STATISTICS dbo.Orders\nSELECT * FROM dbo.Orders",
        "SELECT 'UPDATE STATISTICS dbo.Orders'",
        "UPDATE statistics SET value = 1",
        "UPDATE statistics s SET value = 1",
        "UPDATE statistics AS s SET value = 1",
        "UPDATE statistics_log SET value = 1",
        "UPDATE STATISTICS",
        "UPDATE STATISTICS dbo.",
        "UPDATE STATISTICS dbo.Orders /* unclosed",
        "UPDATE STATISTICS \"dbo\".\"Orders\"",
    ] {
        assert_eq!(mssql_statistics_target(text), None, "{text}");
    }
}

#[test]
fn a_recompile_hint_is_code_in_the_option_list() {
    for text in [
        "SELECT * FROM dbo.orders WHERE id = @P1 OPTION (RECOMPILE)",
        "select * from dbo.orders option (maxdop 1, /* why */ recompile, force order);",
        "SELECT * FROM dbo.orders OPTION (USE HINT ('DISABLE_PARAMETER_SNIFFING'), RECOMPILE)",
    ] {
        assert!(has_mssql_recompile_hint(text), "{text}");
    }
    for text in [
        "SELECT 'OPTION (RECOMPILE)' FROM dbo.orders",
        "SELECT \"OPTION (RECOMPILE)\", [RECOMPILE] FROM dbo.orders",
        "SELECT * FROM dbo.orders /* OPTION (RECOMPILE) */ OPTION (MAXDOP 1)",
        "SELECT * FROM dbo.orders -- OPTION (RECOMPILE)",
        "SELECT * FROM dbo.orders OPTION (USE HINT ('RECOMPILE'))",
        "SELECT * FROM dbo.orders OPTION (OPTIMIZE FOR (@RECOMPILE = 1))",
        "SELECT * FROM dbo.orders OPTION (RECOMPILE) /* unclosed",
    ] {
        assert!(!has_mssql_recompile_hint(text), "{text}");
    }
}

#[test]
fn a_limit_says_its_count_where_the_text_writes_it() {
    let m = Engine::Mysql;
    assert_eq!(limit_count("SELECT * FROM orders WHERE promo_code = 'X' LIMIT 50", m), Some(50.0));
    assert_eq!(limit_count("SELECT * FROM orders LIMIT 10, 50", m), Some(50.0));
    assert_eq!(limit_count("select * from orders limit 50 offset 10", Engine::Postgres), Some(50.0));
    assert_eq!(limit_count("SELECT * FROM `orders` WHERE `promo_code` = ? LIMIT ?", m), None);
    assert!(has_limit("SELECT * FROM `orders` WHERE `promo_code` = ? LIMIT ?", m));
    assert_eq!(limit_count("SELECT rate_limit FROM plans", m), None);
    assert!(!has_limit("SELECT rate_limit, limit_x FROM plans", m));
    // A LIMIT inside a string or a comment, or a subquery's, isn't the statement's: a number written there is data.
    assert_eq!(limit_count("SELECT * FROM notes WHERE body = 'LIMIT 4242'", m), None);
    assert_eq!(limit_count("SELECT * FROM notes LIMIT 5 /* LIMIT 4242 */", m), Some(5.0));
    assert_eq!(limit_count("SELECT * FROM notes LIMIT 5 -- x, 4242", m), Some(5.0));
    assert_eq!(limit_count("SELECT * FROM t WHERE id IN (SELECT id FROM u LIMIT 7)", m), None);
    assert_eq!(limit_count("SELECT * FROM notes LIMIT /* 4242 */ 5", m), None, "only a count right after it");
    assert_eq!(limit_count("SELECT * FROM notes LIMIT 99999999999", m), None, "not a page size");
    assert_eq!(limit_count("SELECT * FROM notes LIMIT 'x", m), None, "text the lexer can't read");
}

#[test]
fn session_statements_are_the_drivers_not_the_applications() {
    for text in [
        "BEGIN",
        "COMMIT;",
        "START TRANSACTION",
        "SAVEPOINT active_record_1",
        "SET autocommit = ?",
        "SET NAMES utf8mb4",
        "SHOW WARNINGS",
        "DISCARD ALL",
        "/* ping */ SELECT 1",
        "SELECT ?",
        "select $1",
        "PRAGMA foreign_keys = ?",
    ] {
        assert!(is_session_statement(text), "{text}");
    }
    for text in [
        "SELECT 1 FROM orders WHERE id = ?",
        "SELECT id FROM users",
        "SET NOCOUNT ON; SELECT * FROM t",
        "STARTUP",
        "UPDATE t SET a = 1",
        "SELECT count(*)",
    ] {
        assert!(!is_session_statement(text), "{text}");
    }
}

#[test]
fn a_cte_update_and_its_set_clause_are_one_statement() {
    let one = |text: &str| single_statement(text, Engine::Mssql).is_ok();
    // A queue claim: take rows other sessions aren't holding, mark them, return them.
    assert!(one("WITH due AS (SELECT TOP (50) id, next_attempt_at FROM q WITH (UPDLOCK, READPAST, ROWLOCK) \
         WHERE status = 'pending' ORDER BY next_attempt_at) \
         UPDATE due SET next_attempt_at = SYSUTCDATETIME() OUTPUT inserted.id"));
    assert!(one("WITH c AS (SELECT id FROM t) MERGE t USING c ON t.id = c.id WHEN MATCHED THEN UPDATE SET x = 1;"));
    assert!(one("MERGE t USING s ON t.id = s.id WHEN MATCHED AND s.gone = 1 THEN UPDATE SET t.x = 0 \
         WHEN MATCHED THEN UPDATE SET t.x = s.x;"));
    // Only the SET of an UPDATE is a clause: any other SET is another statement.
    assert!(!one("WITH c AS (SELECT 1 AS a) SELECT a FROM c SET NOCOUNT ON"));
    assert!(!one("WITH c AS (SELECT 1 AS a) UPDATE t SET x = 1 SET NOCOUNT ON"));
    assert!(!one("UPDATE t SET x = 1 SET NOCOUNT ON"));
    assert!(one("UPDATE t SET x = (SELECT 1), y = 2 WHERE id = 3"));
}

#[test]
fn a_tsql_if_and_its_body_are_one_statement() {
    let one = |text: &str| single_statement(text, Engine::Mssql).is_ok();
    assert!(one(
        "IF NOT EXISTS (SELECT 1 FROM sys.database_principals WHERE name = 'reader') CREATE USER [reader] FROM EXTERNAL PROVIDER;"
    ));
    assert!(one("IF OBJECT_ID('dbo.t') IS NULL CREATE TABLE dbo.t (id int)"));
    assert!(one("IF EXISTS (SELECT 1 FROM a) INSERT INTO b SELECT * FROM a"));
    assert!(!one("IF 1 = 1 SELECT 1 SELECT 2"));
    assert!(!one("IF 1 = 1 BEGIN SELECT 1 SELECT 2 END"));
    assert!(!one("IF 1 = 1 SELECT 1 ELSE DELETE FROM t"));
    // Nesting is capped: a deep chain is refused at once instead of recursing without end.
    assert!(one(&format!("{}SELECT 1", "IF 1 = 1 ".repeat(8))));
    assert!(!one(&format!("{}SELECT 1", "IF 1 = 1 ".repeat(9))));
    assert!(!one(&format!("{}SELECT 1", "IF 1 = 1 ".repeat(200_000))));
}

#[test]
fn tables_a_statement_touches() {
    assert_eq!(
        tables_in(
            "SELECT * FROM public.orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = $1",
            Engine::Postgres
        ),
        vec!["public.orders", "customers"]
    );
    assert_eq!(
        tables_in("select a from [dbo].[Order Lines] l, dbo.Items AS i with (nolock)", Engine::Mssql),
        vec!["dbo.order lines", "dbo.items"]
    );
    assert_eq!(
        tables_in("INSERT INTO shop.events (a, b) SELECT a, b FROM `staging`.`events`", Engine::Mysql),
        vec!["shop.events", "staging.events"]
    );
    assert_eq!(
        tables_in("SELECT * FROM generate_series(1, 3) g JOIN LATERAL (SELECT 1) x ON true", Engine::Postgres),
        Vec::<String>::new()
    );
    assert_eq!(tables_in("UPDATE ONLY accounts SET n = n + 1 WHERE id = $1", Engine::Postgres), vec!["accounts"]);
    assert_eq!(tables_in("SELECT 1 FROM db.dbo.t", Engine::Mssql), vec!["dbo.t"]);
    assert_eq!(tables_in("INSERT dbo.hot (v) VALUES (@i)", Engine::Mssql), vec!["dbo.hot"]);
    assert_eq!(tables_in("DELETE FROM jobs WHERE id = $1", Engine::Postgres), vec!["jobs"]);
    assert!(tables_in("SELECT 'unterminated FROM x", Engine::Postgres).is_empty());
}

const ALL: [Engine; 3] = [Engine::Mssql, Engine::Postgres, Engine::Mysql];

#[test]
fn one_statement_with_trailing_noise_is_fine() {
    for engine in ALL {
        assert_eq!(single_statement("  SELECT 1 ;; ", engine).unwrap(), "SELECT 1");
        assert_eq!(single_statement("SELECT 1; -- done\n", engine).unwrap(), "SELECT 1");
        assert_eq!(single_statement("SELECT 'a;b' AS x", engine).unwrap(), "SELECT 'a;b' AS x");
        assert_eq!(single_statement("SELECT 1 /* a ; b */ + 1", engine).unwrap(), "SELECT 1 /* a ; b */ + 1");
        assert!(single_statement("  ; ", engine).is_err());
        assert!(single_statement("-- only a comment", engine).is_err());
    }
}

#[test]
fn tsql_statements_without_semicolons_are_refused() {
    // SQL Server runs these as several statements: none may pass as one read.
    for text in [
        "SELECT 1 AS a COMMIT CREATE TABLE tempdb.dbo.x (a int)",
        "SELECT 1 EXEC sp_who",
        "SELECT 1 EXECUTE('DROP TABLE t')",
        "SELECT name FROM sys.tables SELECT 2",
        "SELECT 1 DECLARE @x int",
        "SELECT 1 SET NOCOUNT ON",
        "SELECT 1\nGO\nDROP TABLE t",
        "SELECT 1 DBCC FREEPROCCACHE",
        "SELECT 1 KILL 53",
        "SELECT 1 USE master",
        "SELECT 1 WAITFOR DELAY '00:00:10'",
        "SELECT a FROM t DELETE FROM t",
        "SELECT 1 BEGIN TRAN",
        "SELECT 1 IF 1 = 1 DROP TABLE t",
        "WITH x AS (SELECT 1 AS a) SELECT a FROM x SELECT 2",
        "SELECT 1 GRANT SELECT ON t TO public",
        "GRANT SELECT ON t TO u DROP TABLE t",
        "(@P1 int)SELECT a FROM t WHERE b = @P1 DELETE FROM t",
        // Parser differentials: a number before the dot, unbalanced parentheses.
        "SELECT 1.COMMIT CREATE TABLE tempdb.dbo.x (a int)",
        "SELECT 1. EXEC sp_who",
        "SELECT 1e.EXEC sp_who",
        "SELECT 1) EXEC sp_who",
    ] {
        assert!(single_statement(text, Engine::Mssql).is_err(), "{text}");
        assert!(!reads_only(text, Engine::Mssql), "{text}");
        assert!(check(text, Engine::Mssql).problem.is_some(), "{text}");
    }
    // One statement each, with the words that legitimately appear inside one.
    for (text, read) in [
        ("SELECT a FROM t UNION ALL SELECT b FROM u", true),
        ("SELECT a FROM t EXCEPT SELECT b FROM u", true),
        ("WITH x AS (SELECT 1 AS a) SELECT a FROM x", true),
        ("SELECT a FROM t WITH (NOLOCK)", true),
        ("SELECT TOP 5 WITH TIES a FROM t ORDER BY a", true),
        ("SELECT a FROM t INNER MERGE JOIN u ON t.id = u.id", false),
        ("SELECT t.[exec], t.[begin] FROM t", true),
        ("SELECT x.exec FROM t AS x", true),
        ("SELECT a FROM t WHERE b = @begin", true),
        ("SELECT a FROM t ORDER BY a OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY", true),
        ("SELECT a FROM t OPTION (USE HINT ('FORCE_LEGACY_CARDINALITY_ESTIMATION'))", true),
        ("UPDATE t SET a = 1 WHERE b = 2", false),
        (
            "MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id)",
            false,
        ),
        ("INSERT INTO t (a) SELECT a FROM u", false),
        ("INSERT INTO t EXEC p", false),
        ("WITH x AS (SELECT 1 AS a) DELETE FROM t WHERE a IN (SELECT a FROM x)", false),
        ("ALTER TABLE t DROP COLUMN c", false),
        ("DROP TABLE IF EXISTS t", false),
        ("ALTER DATABASE d SET SINGLE_USER WITH ROLLBACK IMMEDIATE", false),
        ("GRANT SELECT ON t TO u WITH GRANT OPTION", false),
        ("CREATE PROCEDURE p AS BEGIN SELECT 1 END", false),
        ("CREATE OR ALTER PROCEDURE p AS BEGIN SET NOCOUNT ON SELECT 1 END", false),
        ("DENY EXECUTE ON SCHEMA::app TO u", false),
        ("CREATE NONCLUSTERED INDEX ix ON t (a) WITH (ONLINE = ON)", false),
        ("(@P1 nvarchar(4000))SELECT id FROM shop.customers WHERE email = @P1", false),
    ] {
        assert!(single_statement(text, Engine::Mssql).is_ok(), "{text}");
        assert_eq!(reads_only(text, Engine::Mssql), read, "{text}");
    }
}

#[test]
fn every_server_setting_must_agree() {
    // Under MySQL's default mode `"a\" , …"` is one string; under ANSI_QUOTES `"a\"` is a name and SLEEP is a call.
    let text = "SELECT \"a\\\" , SLEEP(5) -- \"";
    assert!(!reads_only(text, Engine::Mysql), "{text}");
    // SQL Server with QUOTED_IDENTIFIER OFF reads `"…"` as a string: both readings still see one read.
    assert!(reads_only("SELECT \"x\" AS a FROM t", Engine::Mssql));
    assert!(reads_only("SELECT \"col\" FROM t", Engine::Mysql));
}

#[test]
fn a_setting_where_the_text_never_parses_does_not_refuse_a_read() {
    // Valid in the server's default mode; under the other backslash setting the quote never closes, so a server
    // running that way rejects the whole text and nothing runs.
    assert!(reads_only(r"SELECT 'it\'s' AS s", Engine::Mysql));
    assert!(reads_only(r#"SELECT "a\"b" AS s"#, Engine::Mysql));
    assert!(reads_only(r"SELECT 'C:\' AS p", Engine::Postgres));
    // But MySQL runs multi-statement text one statement at a time: with backslashes literal, this is a SELECT, a
    // DELETE and an open quote, and the first two would run before the error.
    let text = r"SELECT 'a\'; DELETE FROM t; SELECT '";
    assert!(single_statement(text, Engine::Mysql).is_err(), "{text}");
    assert!(!reads_only(text, Engine::Mysql), "{text}");
    // Errors that don't depend on settings refuse the text in every reading.
    assert!(!reads_only("SELECT 1 /* open", Engine::Mysql));
    assert!(!reads_only("SELECT\u{a0}1", Engine::Postgres));
}

#[test]
fn a_cursor_fetch_is_a_statement_but_offset_fetch_is_part_of_the_query() {
    assert!(!reads_only("SELECT 1 AS a\rFETCH NEXT FROM c", Engine::Mssql));
    assert!(!reads_only("SELECT name FROM sys.tables\u{c}FETCH NEXT FROM c", Engine::Mssql));
    let paged = "SELECT name FROM sys.tables ORDER BY name OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY";
    assert!(reads_only(paged, Engine::Mssql), "{paged}");
    assert!(reads_only("SELECT a FROM t ORDER BY a OFFSET 1 ROW FETCH FIRST 1 ROW ONLY", Engine::Mssql));
    assert!(reads_only("SELECT CASE WHEN a = 1 THEN 'x' END AS b FROM t", Engine::Mssql));
    assert!(!reads_only("SELECT 1 AS a END CONVERSATION @h", Engine::Mssql));
    assert!(!reads_only("SELECT 1 AS a BREAK", Engine::Mssql));
}

#[test]
fn no_tsql_statement_keyword_hides_behind_any_separator() {
    let separators = [" ", "\n", "\t", "\r", "\r\n", "/**/", "/* note */", " -- note\n", " -- note\r", ".", ") "];
    for keyword in MSSQL_STATEMENTS.iter().filter(|k| **k != "WITH") {
        // The words that are statements only in context get it: END CONVERSATION, ADD SIGNATURE.
        let keyword = match *keyword {
            "END" => "END CONVERSATION",
            "ADD" => "ADD SIGNATURE",
            k => k,
        };
        for sep in separators {
            let text = format!("SELECT 1{sep}{keyword} x");
            assert!(!reads_only(&text, Engine::Mssql), "{text:?}");
            assert!(single_statement(&text, Engine::Mssql).is_err(), "{text:?}");
        }
    }
}

#[test]
fn a_carriage_return_ends_a_line_comment() {
    assert!(!reads_only("SELECT 1 -- note\r, pg_stat_statements_reset()", Engine::Postgres));
    assert!(single_statement("SELECT 1 -- note\rCOMMIT CREATE TABLE t (a int)", Engine::Mssql).is_err());
    assert!(!reads_only("SELECT 1 # note\r, GET_LOCK('a', 10)", Engine::Mysql));
    // An ordinary comment on its own line still reads.
    assert!(reads_only("SELECT 1 -- note\r\n", Engine::Postgres));
}

#[test]
fn invisible_characters_never_hide_a_keyword() {
    for (text, engine) in [
        ("SELECT 1\u{00A0}EXEC sp_who", Engine::Mssql),
        ("SELECT 1\u{200B}EXEC sp_who", Engine::Mssql),
        ("SELECT 1\u{3000}EXEC sp_who", Engine::Mssql),
        ("SELECT 1\u{2028}COMMIT", Engine::Mssql),
        ("SELECT pg_stat_reset\u{FEFF}()", Engine::Postgres),
        ("SELECT 1\u{0000}; DROP TABLE t", Engine::Mysql),
        ("SELECT 1\u{0085}KILL 53", Engine::Mssql),
    ] {
        assert!(single_statement(text, engine).is_err(), "{text:?}");
        assert!(!reads_only(text, engine), "{text:?}");
    }
    // Non-ASCII inside strings, quoted names and plain identifiers is fine.
    assert!(reads_only("SELECT 'café\u{00A0}' AS x, [prénom] FROM t", Engine::Mssql));
    assert!(reads_only("SELECT prénom FROM clients", Engine::Postgres));
    // A real qualified name still reads.
    assert!(reads_only("SELECT t.exec, [t].begin FROM t", Engine::Mssql));
}

#[test]
fn replication_lag_arithmetic_is_a_read() {
    assert!(reads_only(
        "SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn) FROM pg_stat_replication",
        Engine::Postgres
    ));
    assert!(!reads_only("SELECT pg_wal_replay_pause()", Engine::Postgres));
}

#[test]
fn functions_with_side_effects_are_not_reads() {
    for text in [
        "SELECT pg_stat_statements_reset()",
        "SELECT pg_stat_reset_single_table_counters(0)",
        "SELECT pg_stat_reset()",
        "SELECT pg_create_logical_replication_slot('s', 'pgoutput')",
        "SELECT pg_drop_replication_slot('s')",
        "SELECT pg_log_backend_memory_contexts(1)",
        "SELECT * FROM crosstab('SELECT pg_terminate_backend(1)')",
        "SELECT pg_backup_start('x')",
        "SELECT \"pg_stat_statements_reset\"()",
    ] {
        assert!(!reads_only(text, Engine::Postgres), "{text}");
    }
    for text in ["SELECT NEXT VALUE FOR s", "SELECT nextval(s)", "SELECT setval(s, 5)", "SELECT SLEEP(10)"] {
        assert!(!reads_only(text, Engine::Mysql), "{text}");
    }
    // Ordinary functions still read.
    assert!(reads_only("SELECT lower(name), count(*) FROM t GROUP BY 1", Engine::Postgres));
    assert!(reads_only("SELECT pg_size_pretty(pg_total_relation_size('t'))", Engine::Postgres));
}

#[test]
fn stacked_statements_are_refused() {
    for engine in ALL {
        assert!(single_statement("SELECT 1; DROP TABLE t", engine).is_err());
        assert!(single_statement("SELECT 1;\nSET x = 1;", engine).is_err());
        assert!(single_statement("SELECT 'unterminated", engine).is_err());
        assert!(single_statement("SELECT 1 /* open", engine).is_err());
    }
}

#[test]
fn mysql_backslashes_and_comments_follow_the_server() {
    // Under NO_BACKSLASH_ESCAPES the quote closes early and `; DROP` becomes a second statement: refuse either way.
    assert!(single_statement(r"SELECT 'a\'; DROP TABLE t; -- '", Engine::Mysql).is_err());
    assert!(single_statement(r#"SELECT "a\"; DROP TABLE t; -- ""#, Engine::Mysql).is_err());
    // `--` without a following space is arithmetic in MySQL, so the `;` is real.
    assert!(single_statement("SELECT 1--1; DROP TABLE t", Engine::Mysql).is_err());
    assert_eq!(
        single_statement("SELECT 1 -- note; still a comment", Engine::Mysql).unwrap(),
        "SELECT 1 -- note; still a comment"
    );
    assert_eq!(single_statement("SELECT 1 # note; comment", Engine::Mysql).unwrap(), "SELECT 1 # note; comment");
    assert!(single_statement("SELECT 1 /*!80000 ; DROP TABLE t */", Engine::Mysql).is_err());
    assert!(single_statement("SELECT 1 /*M!100000 ; DROP TABLE t */", Engine::Mysql).is_err());
    assert!(single_statement("SELECT /*+ NO_INDEX(t) */ a FROM t", Engine::Mysql).is_ok());
    assert!(single_statement("SELECT `a;b` FROM t", Engine::Mysql).is_ok());
    // MySQL block comments don't nest: the first `*/` ends it.
    assert!(single_statement("SELECT 1 /* /* */ ; DROP TABLE t */", Engine::Mysql).is_err());
}

#[test]
fn postgres_quoting_is_followed() {
    let pg = Engine::Postgres;
    assert!(single_statement("SELECT $$a;b$$, $tag$ ; $tag$", pg).is_ok());
    assert!(single_statement("SELECT * FROM t WHERE id = $1", pg).is_ok());
    assert!(single_statement("SELECT $$never closed; DROP TABLE t", pg).is_err());
    assert!(single_statement(r"SELECT E'a\'; b'", pg).is_ok());
    // With standard_conforming_strings off this closes early; with it on it doesn't: ambiguous, so refused.
    assert!(single_statement(r"SELECT 'a\'; DROP TABLE t; --'", pg).is_err());
    assert!(single_statement("SELECT 1 /* outer /* inner ; */ still */", pg).is_ok());
    assert!(single_statement(r#"SELECT "a;b" FROM t"#, pg).is_ok());
    assert!(single_statement("SELECT 1 # 2; DROP TABLE t", pg).is_err());
}

#[test]
fn mssql_brackets_are_names() {
    assert!(single_statement("SELECT [a;b] FROM [t]]x]", Engine::Mssql).is_ok());
    assert!(single_statement("SELECT * FROM #temp; DROP TABLE t", Engine::Mssql).is_err());
}

#[test]
fn checks_name_the_risk() {
    let pg = Engine::Postgres;
    assert!(check("select 1", pg).reads_only);
    let every_row = check("DELETE FROM orders", pg);
    assert_eq!((every_row.reads_only, every_row.verb.as_str()), (false, "DELETE"));
    assert!(every_row.warning.unwrap().contains("every row"));
    assert!(check("UPDATE orders SET note = '' WHERE id = 1", pg).warning.is_none());
    assert!(check("DROP TABLE t", Engine::Mssql).warning.unwrap().contains("removes"));
    assert!(check("SELECT * FROM t FOR UPDATE", pg).warning.unwrap().contains("can't prove"));
    assert!(check("SELECT 1; SELECT 2", pg).problem.is_some());
}

#[test]
fn reads_are_told_from_writes() {
    for engine in ALL {
        assert!(reads_only("SELECT a, b FROM t WHERE c = 'DELETE'", engine));
        assert!(reads_only("  (SELECT 1)", engine));
        assert!(!reads_only("DELETE FROM t", engine));
        assert!(!reads_only("UPDATE t SET a = 1", engine));
        assert!(!reads_only("SELECT a INTO t2 FROM t", engine));
        assert!(!reads_only("SELECT 1; DELETE FROM t", engine));
        assert!(!reads_only("WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone", engine));
        assert!(!reads_only("SELECT * FROM t FOR UPDATE", engine));
        assert!(!reads_only("EXEC sp_who", engine));
        assert!(!reads_only("", engine));
    }
    assert!(reads_only("WITH x AS (SELECT 1) SELECT * FROM x", Engine::Mssql));
    assert!(!reads_only("SELECT * FROM t FOR SHARE", Engine::Postgres));
    assert!(!reads_only("SELECT * FROM t LOCK IN SHARE MODE", Engine::Mysql));
    assert!(!reads_only("SELECT NEXT VALUE FOR dbo.seq", Engine::Mssql));
    assert!(!reads_only("SELECT * FROM t WITH (UPDLOCK) WHERE id = 1", Engine::Mssql));
    assert!(!reads_only("SELECT * FROM OPENQUERY(remote, 'DELETE FROM t')", Engine::Mssql));
    assert!(!reads_only("SELECT pg_terminate_backend(123)", Engine::Postgres));
    assert!(!reads_only("SELECT set_config('default_transaction_read_only', 'off', false)", Engine::Postgres));
    assert!(!reads_only("SELECT pg_advisory_lock(1)", Engine::Postgres));
    assert!(!reads_only("SELECT query_to_xml('DELETE FROM t', true, true, '')", Engine::Postgres));
    assert!(!reads_only("EXPLAIN ANALYZE SELECT * FROM t", Engine::Postgres));
    assert!(reads_only("EXPLAIN SELECT * FROM t", Engine::Postgres));
    assert!(reads_only("SHOW work_mem", Engine::Postgres));
    assert!(!reads_only("SHOW work_mem", Engine::Mssql));
    assert!(!reads_only("SELECT GET_LOCK('x', 10)", Engine::Mysql));
    assert!(reads_only("SELECT nextval_count FROM stats", Engine::Postgres));
    assert!(reads_only("SELECT `update` FROM t", Engine::Mysql));
    assert!(!reads_only("SELECT 1 /*!80000 , (DELETE FROM t) */", Engine::Mysql));
    // Quoted names call functions too.
    assert!(!reads_only(r#"SELECT "pg_terminate_backend"(123)"#, Engine::Postgres));
    assert!(!reads_only(r#"SELECT pg_catalog."set_config"('a', 'b', false)"#, Engine::Postgres));
    assert!(!reads_only(r#"SELECT U&"pg_terminate_backend"(1)"#, Engine::Postgres));
    assert!(!reads_only("SELECT * FROM [openquery](remote, 'DELETE FROM t')", Engine::Mssql));
    assert!(!reads_only("SELECT `get_lock`('a', 1)", Engine::Mysql));
    assert!(!reads_only(r#"SELECT "get_lock"('a', 1)"#, Engine::Mysql), "an identifier under ANSI_QUOTES");
    assert!(reads_only(r#"SELECT "Name", [Order] FROM t"#, Engine::Mssql));
    assert!(reads_only(r#"SELECT "name" FROM "orders" WHERE "id" = 1"#, Engine::Postgres));
    assert!(!reads_only("SELECT * FROM t FOR KEY SHARE", Engine::Postgres));
}

fn mark(sql: &str) -> String {
    mark_mssql_statements(sql, "/* culprit */")
}

#[test]
fn marks_every_statement_keyword() {
    assert_eq!(mark("SELECT a FROM t"), "SELECT /* culprit */ a FROM t");
    assert_eq!(
        mark("INSERT INTO t (a) SELECT a FROM s; DELETE FROM s;\nexec dbo.refresh"),
        "INSERT /* culprit */ INTO t (a) SELECT /* culprit */ a FROM s; DELETE /* culprit */ FROM s;\nexec /* culprit */ dbo.refresh"
    );
    assert_eq!(
        mark("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a;"),
        "MERGE /* culprit */ t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE /* culprit */ SET a = s.a;"
    );
    assert_eq!(mark("EXECUTE('SELECT 1')"), "EXECUTE /* culprit */('SELECT 1')");
    // A common table expression's statement is marked, and so is its own SELECT.
    assert_eq!(
        mark("WITH old AS (SELECT id FROM t WHERE done = 1) UPDATE old SET archived = 1"),
        "WITH old AS (SELECT /* culprit */ id FROM t WHERE done = 1) UPDATE /* culprit */ old SET archived = 1"
    );
    // A batch that declares first: the SELECT after it is the statement Query Store keeps.
    assert_eq!(
        mark(
            "DECLARE @since datetime2 = DATEADD(day, -1, SYSUTCDATETIME());\nSELECT COUNT(*) FROM t WHERE at > @since"
        ),
        "DECLARE @since datetime2 = DATEADD(day, -1, SYSUTCDATETIME());\nSELECT /* culprit */ COUNT(*) FROM t WHERE at > @since"
    );
}

#[test]
fn never_marks_inside_strings_names_or_comments() {
    // The value written stays the person's.
    assert_eq!(mark("UPDATE t SET note = 'select'"), "UPDATE /* culprit */ t SET note = 'select'");
    assert_eq!(mark("UPDATE t SET note = N'it''s a delete'"), "UPDATE /* culprit */ t SET note = N'it''s a delete'");
    assert_eq!(mark("-- delete me\nSELECT 1"), "-- delete me\nSELECT /* culprit */ 1");
    assert_eq!(
        mark("/* select /* nested update */ exec */ SELECT 1"),
        "/* select /* nested update */ exec */ SELECT /* culprit */ 1"
    );
    assert_eq!(
        mark("SELECT [select], [a]]update] FROM [delete]"),
        "SELECT /* culprit */ [select], [a]]update] FROM [delete]"
    );
    assert_eq!(mark(r#"SELECT "update" FROM t"#), r#"SELECT /* culprit */ "update" FROM t"#);
    assert_eq!(mark("SELECT @select, selected FROM #select"), "SELECT /* culprit */ @select, selected FROM #select");
}

#[test]
fn leaves_text_without_a_statement_keyword_or_with_a_module_body_as_it_is() {
    for sql in [
        "DECLARE @x int = 1",
        "SET NOCOUNT ON",
        "TRUNCATE TABLE t",
        "CREATE PROCEDURE dbo.p AS SELECT 1",
        "create or alter view dbo.v as select a from t",
        "ALTER FUNCTION dbo.f() RETURNS TABLE AS RETURN (SELECT 1 AS a)",
        "CREATE TRIGGER tr ON t AFTER INSERT AS UPDATE t SET a = 1",
        // Text the lexer can't read is sent as written.
        "SELECT 'never closed",
    ] {
        assert_eq!(mark(sql), sql);
    }
}

#[test]
fn a_parameterized_statement_starts_after_its_parameter_list() {
    assert_eq!(
        without_parameter_list("(@par0 nvarchar(4000))update [app].[conversations] set n = n + 1"),
        "update [app].[conversations] set n = n + 1"
    );
    assert_eq!(without_parameter_list("  (@P1 int,@P2 decimal(10, 2))SELECT 1"), "SELECT 1");
    assert_eq!(without_parameter_list("(SELECT 1) UNION (SELECT 2)"), "(SELECT 1) UNION (SELECT 2)");
    assert_eq!(without_parameter_list("(@P1 int"), "(@P1 int");
}

#[test]
fn compound_updates_are_never_isolated_allocators() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
        assert!(!isolated_counter_update(
            "UPDATE orders SET n = n + 1, changed_at = CURRENT_TIMESTAMP RETURNING n",
            engine,
            "n"
        ));
        assert!(!isolated_counter_update("UPDATE orders SET n = n + 1 RETURNING n, status", engine, "n"));
        assert!(!isolated_counter_update("UPDATE orders SET status = 1 RETURNING n", engine, "n"));
    }
    assert!(isolated_counter_update(
        "UPDATE orders SET n = n + 1 OUTPUT inserted.n WHERE id = @id",
        Engine::Mssql,
        "n"
    ));
    assert!(isolated_counter_update("UPDATE orders SET n = n + 1 RETURNING n", Engine::Postgres, "n"));
    assert!(isolated_counter_update("UPDATE orders SET n = LAST_INSERT_ID(n + 1) WHERE id = ?", Engine::Mysql, "n"));
    assert!(isolated_counter_update("UPDATE orders SET n = n + 1 RETURNING n", Engine::Sqlite, "n"));
}

#[test]
fn cte_targets_resolve_only_unambiguous_sources_on_every_engine() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
        assert_eq!(
            cte_write_target(
                "/* task */ WITH pending AS (SELECT * FROM shop.orders WHERE state = 0) UPDATE pending SET state = 1",
                engine
            )
            .as_deref(),
            Some("shop.orders")
        );
        assert_eq!(
            cte_write_target("WITH a AS (SELECT * FROM orders), b AS (SELECT * FROM a) DELETE FROM b", engine)
                .as_deref(),
            Some("orders")
        );
        for sql in [
            "WITH a AS (SELECT * FROM orders o JOIN products p ON p.id = o.id) UPDATE a SET state = 1",
            "WITH a AS (SELECT * FROM orders o, products p) UPDATE a SET state = 1",
            "WITH a AS (SELECT * FROM a) UPDATE a SET state = 1",
            "WITH a AS (SELECT * FROM orders UNION SELECT * FROM products) UPDATE a SET state = 1",
        ] {
            assert_eq!(cte_write_target(sql, engine), None, "{engine:?}: {sql}");
        }
    }
    assert_eq!(
        cte_write_target(
            "WITH [pending] AS (SELECT TOP (10) * FROM [shop].[orders] WITH (UPDLOCK, READPAST)) UPDATE [pending] SET state = 1",
            Engine::Mssql
        )
        .as_deref(),
        Some("shop.orders")
    );
}

#[test]
fn extract_call_site_reads_marginalia_sqlcommenter_and_file_line_comments() {
    assert_eq!(
        extract_call_site(
            "SELECT * FROM orders WHERE customer_id = $1 /*application='shop',controller='orders',action='index',line='app/models/order.rb:42:in `recent`'*/"
        )
        .as_deref(),
        Some("app/models/order.rb:42:in 'recent' (controller=orders, action=index)")
    );
    assert_eq!(
        extract_call_site("/* file='src/orders/repo.ts', line='88', function='listOrders' */ SELECT * FROM orders")
            .as_deref(),
        Some("src/orders/repo.ts:88 (function=listOrders)")
    );
    assert_eq!(
        extract_call_site("-- app/services/checkout.py:142\nSELECT * FROM orders WHERE id = $1").as_deref(),
        Some("app/services/checkout.py:142")
    );
    assert_eq!(extract_call_site("SELECT '/* app/fake.py:10 */' FROM orders /* culprit */"), None);
    assert_eq!(extract_call_site("SELECT $$/* app/fake.py:10 */$$ FROM orders"), None);
    assert_eq!(extract_call_site("/* https://example.com/app.js:10 */ SELECT 1"), None);
}
