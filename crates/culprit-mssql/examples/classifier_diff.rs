//! Differential test of Culprit's read-only classifier against SQL Server's own parser. Builds batches from fragments
//! (reads, statements that write, comments, quotes, separators, odd characters), and for every batch the classifier
//! accepts as one read, asks the server how it reads the text under SHOWPLAN (compiled, never executed). A batch the
//! classifier accepts but the server reads as anything other than exactly one SELECT is a classifier gap. The engine's
//! SHOWPLAN guard still refuses those before anything runs; this finds them so the classifier can be fixed too.
//! Development aid: run against a throwaway server only.
//!
//! CULPRIT_MSSQL_ADO="Server=127.0.0.1,14333;Database=culprit_demo;User Id=sa;Password=…;TrustServerCertificate=true" \
//!   cargo run -p culprit-mssql --example classifier_diff -- [batches] [seed]

use culprit_core::{Engine, Provider, sql};
use culprit_mssql::{MssqlProvider, parse_ado};

/// Reads the classifier must keep accepting.
const READS: &[&str] = &[
    "SELECT 1 AS a",
    "SELECT name FROM sys.tables",
    "SELECT 'x' AS s",
    "SELECT N'y' AS s",
    "WITH c AS (SELECT 1 AS a) SELECT a FROM c",
    "SELECT [select] FROM (SELECT 1 AS [select]) AS t",
    "SELECT \"a\" FROM (SELECT 1 AS a) AS t",
    "SELECT TOP 1 name FROM sys.objects ORDER BY name",
    "SELECT COUNT(*) FROM sys.columns OPTION (RECOMPILE)",
    "SELECT 1.5e0 AS n",
    "SELECT name FROM sys.tables ORDER BY name OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
    "SELECT CASE WHEN 1 = 1 THEN 'end' END AS e",
];

/// Statements that must never ride along with a read. Harmless even if run: temp tables, `WHERE 1 = 0`, no-ops.
const OTHERS: &[&str] = &[
    "DELETE FROM #t",
    "UPDATE sys_none SET a = 1 WHERE 1 = 0",
    "INSERT INTO #t VALUES (1)",
    "CREATE TABLE #t (a int)",
    "DROP TABLE #t",
    "TRUNCATE TABLE #t",
    "SELECT 1 AS a INTO #t",
    "EXEC sp_who",
    "EXECUTE ('SELECT 1')",
    "DECLARE @x int",
    "SET NOCOUNT ON",
    "WAITFOR DELAY '00:00:00'",
    "BEGIN TRAN",
    "COMMIT",
    "ROLLBACK",
    "SAVE TRAN s",
    "IF 1 = 1 SELECT 1",
    "WHILE 1 = 0 BREAK",
    "PRINT 'x'",
    "RAISERROR('x', 0, 1)",
    "MERGE #t AS t USING (SELECT 1 AS a) AS s ON 1 = 0 WHEN NOT MATCHED THEN INSERT (a) VALUES (s.a);",
    "GRANT SELECT ON sys.objects TO public",
    "OPEN c",
    "FETCH NEXT FROM c",
    "CHECKPOINT",
    "DBCC FREEPROCCACHE",
    "USE master",
    "CLOSE c",
    "DEALLOCATE c",
    "DECLARE c CURSOR FOR SELECT 1",
    "BREAK",
    "CONTINUE",
    "LINENO 1",
    "RETURN",
    "GOTO x",
    "x:",
    "THROW 50000, 'x', 1",
    "REVERT",
    "SETUSER",
    "ENABLE TRIGGER t ON #t",
    "UPDATE STATISTICS #t",
    "BEGIN TRY SELECT 1 END TRY BEGIN CATCH SELECT 2 END CATCH",
    "BEGIN DISTRIBUTED TRANSACTION",
    "END CONVERSATION @h",
    "SEND ON CONVERSATION @h",
    "RECEIVE * FROM q",
    "GET CONVERSATION GROUP @g FROM q",
    "MOVE CONVERSATION @h TO @g",
    "READTEXT t.c @p 0 1",
    "WRITETEXT t.c @p 'x'",
    "UPDATETEXT t.c @p 0 0 'x'",
    "BULK INSERT #t FROM 'x'",
    "ADD SIGNATURE TO p BY CERTIFICATE c",
    "EXECUTE AS USER = 'dbo'",
    "SELECT 1 AS a; SELECT 2 AS b",
];

/// Glue between fragments, including the characters lexers disagree on.
const GLUE: &[&str] = &[
    " ",
    "\n",
    "\r",
    "\r\n",
    "\t",
    "\u{0b}",
    "\u{0c}",
    "\u{85}",
    "\u{a0}",
    "\u{2028}",
    "\u{3000}",
    "/* c */",
    "/*/* nested */*/",
    "/* a */ /* b */",
    "-- c\n",
    "-- c\r",
    "-- c\r\n",
    "--\n",
    ";",
    " ; ",
    ")",
    "(",
    ".",
    "'",
    "''",
    "\"",
    "[",
    "]",
    "]]",
    "N'",
    "0x",
    "1e",
    "1.",
    ".5",
    "$",
    "@",
    "#",
    "\\",
    "*/",
    "/*",
    "GO",
    "\nGO\n",
];

/// The statement types SQL Server's showplan reports for a read (mirrors the engine's allow-list).
const READ_TYPES: [&str; 4] = ["SELECT", "SELECT WITHOUT QUERY", "XML SELECT", "JSON SELECT"];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[(self.next() % from.len() as u64) as usize]
    }
}

fn batch(rng: &mut Rng) -> String {
    let mut text = String::new();
    // A read first (what an agent would claim to send), then 1–4 more fragments.
    text.push_str(rng.pick(READS));
    for _ in 0..1 + rng.next() % 4 {
        match rng.next() % 3 {
            0 => text.push_str(rng.pick(OTHERS)),
            _ => text.push_str(rng.pick(GLUE)),
        }
        if rng.next().is_multiple_of(2) {
            text.push_str(rng.pick(GLUE));
        }
    }
    if rng.next().is_multiple_of(3) {
        text.push_str(rng.pick(OTHERS));
    }
    text
}

#[tokio::main]
async fn main() {
    let ado = std::env::var("CULPRIT_MSSQL_ADO").expect("set CULPRIT_MSSQL_ADO");
    let mut args = std::env::args().skip(1);
    let batches: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(5_000);
    let seed: u64 = args.next().and_then(|n| n.parse().ok()).unwrap_or(0x5eed_c0de);
    let provider = MssqlProvider::new(&parse_ado(&ado).expect("valid ADO string")).expect("valid spec");

    // The reads alone must pass both: otherwise the run proves nothing.
    for read in READS {
        assert!(sql::reads_only(read, Engine::Mssql), "classifier refuses a plain read: {read}");
        let types = provider.statement_types(read).await.expect("server reads a plain read").unwrap_or_default();
        assert!(types.len() == 1 && READ_TYPES.contains(&types[0].as_str()), "{read}: {types:?}");
    }

    let mut rng = Rng(seed.max(1));
    let (mut accepted, mut agreed, mut server_error, mut gaps) = (0, 0, 0, Vec::new());
    for _ in 0..batches {
        let text = batch(&mut rng);
        if sql::single_statement(&text, Engine::Mssql).is_err() || !sql::reads_only(&text, Engine::Mssql) {
            continue;
        }
        accepted += 1;
        match provider.statement_types(&text).await {
            Ok(Some(types)) if types.len() == 1 && READ_TYPES.contains(&types[0].as_str()) => agreed += 1,
            Ok(Some(types)) => gaps.push((text, types)),
            Ok(None) => panic!("the login needs SHOWPLAN permission for this test"),
            // The server can't compile it at all, so nothing in it could run.
            Err(_) => server_error += 1,
        }
    }
    println!(
        "{batches} batches · classifier accepted {accepted} · server agreed {agreed} · server rejected the text {server_error} · gaps {}",
        gaps.len()
    );
    for (text, types) in &gaps {
        println!("GAP {text:?} → {types:?}");
    }
    if !gaps.is_empty() {
        std::process::exit(1);
    }
}
