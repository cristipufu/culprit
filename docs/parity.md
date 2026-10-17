# Engine parity

What each feature reads on each engine, and where an engine has nothing to read. MariaDB runs on the MySQL provider
(`engine = "mysql"`); the server's `VERSION()` decides the flavor, and `ServerInfo::name()` / the UI's `serverName()` say
"MariaDB" in sentences, badges and plan headers.

Legend: **✓** same depth as the best engine · **◐** works, with less data (the reason is in the cell) · **–** the engine
keeps nothing to read, and Culprit says so in the UI and CLI.

## Workload and plans

| Feature | SQL Server | PostgreSQL | MySQL | MariaDB |
|---|---|---|---|---|
| Statement statistics | ✓ Query Store (windowed), plan cache fallback | ✓ pg_stat_statements (cumulative) | ✓ performance_schema digests | ✓ performance_schema digests when on; ◐ otherwise the **slow query log table** (`log_output` TABLE, no restart): every logged run, grouped by fingerprint. MySQL with performance_schema off uses the same path |
| Unlock card when nothing is recorded | ✓ ALTER DATABASE SET QUERY_STORE | ✓ CREATE EXTENSION (no restart when preloaded) | ✓ slow log `SET GLOBAL …` first, performance_schema restart second | ✓ one `SET GLOBAL userstat, slow_query_log, log_output, long_query_time` statement first, restart second |
| Plans of captured queries | ✓ Query Store / plan cache | ✓ generic plans (12–15 via PREPARE, 16+ GENERIC_PLAN) | ✓ QUERY_SAMPLE_TEXT (8.0.3+) + history tables + slow-log samples | ◐ history tables (need `events_statements_current` + `_history_long`, off by default: the fix names both) and slow-log samples matched to digests by fingerprint |
| Estimated plan (editor, agents) | ✓ | ✓ | ✓ EXPLAIN FORMAT=JSON | ✓ EXPLAIN FORMAT=JSON (own parser: per-loop rows, `loops` derived before 11.0, rows stand in for cost before 11.0) |
| Actual plan (Analyze) | ✓ STATISTICS XML, rolled back | ✓ EXPLAIN (ANALYZE, BUFFERS) | ✓ EXPLAIN ANALYZE (tree), read-only txn | ✓ ANALYZE FORMAT=JSON, read-only txn: per-operator time, loops, rows, filtered %, pages read, sort spills |
| Last actual plan without running it | ✓ 2019+ LAST_QUERY_PLAN_STATS | – | – | – |
| Plan import | ✓ .sqlplan / XML | ✓ text, JSON | ✓ JSON v1/v2, tree | ✓ EXPLAIN / ANALYZE JSON (detected automatically) |
| Plan insights | ✓ | ✓ | ✓ | ✓ (coverage note in MariaDB's words) |
| Runtime spread / percentiles | ✓ per plan | ◐ min/avg/max/stddev | ✓ histogram (8.0.19+) | ◐ min/avg/max from digests; ✓ exact p50/p95/p99 and histogram from slow-log runs |
| Parameter sensitivity | ✓ two plans side by side | ◐ stddev only (`unstable-query`) | ✓ histogram tail | ◐ slow-log tail; digests give min/avg/max only. Fix uses `ANALYZE TABLE … PERSISTENT FOR COLUMNS` |
| Timeline and trends | ✓ Query Store intervals | ◐ from Culprit's scans | ◐ from Culprit's scans | ◐ scans with performance_schema; ✓ from the slow log's own timestamps without it |
| What changed (two periods) | ✓ any period Query Store keeps | ◐ snaps to scans | ◐ snaps to scans | ◐ scans with performance_schema; ✓ any period the slow log covers without it |
| Query Store tuning recommendations, forced plans | ✓ | – | – | – |
| Statement families | ✓ query_hash | ✓ IN-list/VALUES variants | ✓ digests merge literals | ✓ digests / fingerprints merge literals |

## Live

| Feature | SQL Server | PostgreSQL | MySQL | MariaDB |
|---|---|---|---|---|
| Sessions | ✓ | ✓ | ✓ PROCESSLIST + INNODB_TRX | ✓ PROCESSLIST (ms precision, ALTER/LOAD progress in the state) |
| Blocking chains | ✓ | ✓ pg_blocking_pids | ✓ performance_schema.data_lock_waits | ✓ information_schema.INNODB_LOCK_WAITS / INNODB_LOCKS (was empty before); queued waiters point at the session ahead |
| Idle-in-transaction head's last statement | ✓ | ✓ | ✓ events_statements_current | ◐ events_statements_current/history (consumers off by default) |
| Cancel / kill (non-production) | ◐ KILL only (no cancel exists) | ✓ | ✓ KILL QUERY / KILL | ✓ KILL QUERY / KILL |
| Live progress of a running statement | ✓ per operator (query profiles) | ◐ estimated plan + pg_stat_progress_* | ◐ EXPLAIN FOR CONNECTION + stage progress | ✓ per operator from SHOW ANALYZE (10.9+), plus PROGRESS for ALTER/LOAD |
| Deadlocks | ✓ system_health, every one | ◐ counter only; paste log lines | ◐ latest only (INNODB STATUS) | ◐ latest only (INNODB STATUS, MariaDB's WAITING/CONFLICTING layout parsed) |

## Waits and pressure

| Feature | SQL Server | PostgreSQL | MySQL | MariaDB |
|---|---|---|---|---|
| Where time goes (history) | ✓ Query Store waits per query | ◐ per-statement IO time | ◐ performance_schema totals; lock time per digest | ◐ the same with performance_schema; without it, lock time per statement from the slow log |
| Right now (sample) | ✓ wait-stat deltas | ✓ session sampling | ✓ performance_schema deltas, **process-list sampling** when the instruments record nothing | ✓ same; without performance_schema, process-list sampling (lock waits from INNODB_TRX) |
| Pressure signals | ✓ | ✓ | ✓ | ✓ (SHOW GLOBAL STATUS fallback; was 1 signal without performance_schema) |
| Settings / config checks | ✓ | ✓ | ✓ | ✓ information_schema/SHOW GLOBAL VARIABLES, MariaDB defaults (sort_buffer_size, performance_schema OFF, max_statement_time, userstat) |
| Statement timeout on Culprit's sessions | ◐ lock timeout + client command timeout | ✓ statement_timeout 10 s | ✓ max_execution_time 10 s (SELECT only) | ✓ max_statement_time 10 s (lifted for a write the person chose to run) |

## Schema and indexes

| Feature | SQL Server | PostgreSQL | MySQL | MariaDB |
|---|---|---|---|---|
| Index usage | ✓ dm_db_index_usage_stats | ✓ pg_stat_user_indexes | ✓ performance_schema index IO | ✓ performance_schema; ◐ otherwise **userstat** INDEX_STATISTICS (window: since userstat was turned on, which the server doesn't record) |
| Missing-index requests | ✓ DMVs | – (derived from plans, FKs, scans) | – (derived) | – (derived; measured rows examined vs returned now also qualify a filtered scan, because MariaDB estimates 100% for unindexed filters) |
| Index advisor / cleanup SQL | ✓ DISABLE | ✓ DROP CONCURRENTLY | ✓ INVISIBLE | ✓ IGNORED (10.6+), DROP before |
| Table read/scan counters | ✓ | ✓ seq_scan, dead rows | ✓ rows read without an index | ✓ with performance_schema; – without |
| Catalog / autocomplete | ✓ | ✓ | ✓ | ✓ (MariaSQL dialect in the editor) |

## Editor, sharing, agents

Run (read-only transaction), execute (commits, production confirmation), saved queries, drift, export/share, MCP tools
and the audit log are engine-neutral and verified on all four. `query_plan` (CLI `explain --query-id`, MCP) now falls back
to the plan the scan captured when the normalized text (`WHERE id = ?`) can't be planned again.

## Verified on

SQL Server 2022 (`dev/mssql`), PostgreSQL (`dev/postgres`), MySQL 8.4 (`dev/mysql`), MariaDB 11.8 with
performance_schema and 10.11 with defaults plus the slow log table and userstat (the `mariadb-demo` and `mariadb10-demo`
containers, seeded with `dev/mysql/seed.sql` and its workload). Older versions: see [Versions](#versions).

## Versions

Older servers keep less, and Culprit says what each one can't do instead of failing on it. Verified live on MySQL 5.7.44
(performance_schema on), MariaDB 10.5.29 (defaults: performance_schema off, then the no-restart unlock: userstat, the
slow log to the table at 100 ms) and MariaDB 10.6.28 (performance_schema on), each seeded with `dev/mysql/seed.sql` and
its workload, with a planted blocking chain and a planted deadlock: connect, scan, dashboard, top, explain, analyze,
live, progress, blocking, deadlocks, waits and waits --now, indexes, changes, events, catalog, run and execute, saved
queries, doctor, and every MCP tool.

| Version | What it lacks | What Culprit does |
|---|---|---|
| MySQL 5.7 | TLS with ECDHE (builds on OpenSSL 1.0 negotiate only DHE suites, which rustls doesn't offer) | `ssl-mode=PREFERRED` (the default for local hosts) falls back to plaintext like the mysql client; stricter modes say the server and Culprit share no cipher suite and how to fix it |
| MySQL 5.7 | A sample statement per digest (8.0.3) | Plans come from the statement history; with its consumers off the scan names the `setup_consumers` update that turns them on |
| MySQL 5.7 | EXPLAIN ANALYZE (8.0.18) | Analyze says this version can't measure a run; plan notes say so too |
| MySQL 5.7 | Invisible indexes (8.0) | Unused-index fixes save the definition and drop |
| MySQL 5.7 | `performance_schema.data_lock_waits` (8.0) | Blocking reads `INNODB_LOCK_WAITS` |
| MariaDB 10.5 | Ignored indexes (10.6) | Unused-index fixes save the definition and drop |
| MariaDB 10.5, 10.6 | `SHOW ANALYZE` (10.9) | Progress shows elapsed time and rows examined where the server counts them as it goes |
| MariaDB before 11 | A `nested_loop` array in EXPLAIN JSON (each joined table is another `table` member of one object) | Read member by member, so every table of a join is in the plan; buffered tables count one read per buffer fill |
| MariaDB 10.x | An id for transactions that haven't written (reported as trx 0) | Blocked sessions are matched by the lock they requested, so plain reads are never shown as waiting |
| MariaDB without performance_schema | Wait timing | The dashboard reports the slow log's lock time against database time, not as "where time goes" |

Not verified: MySQL 5.6 and earlier, MariaDB 10.4 and earlier, Percona Server, and MySQL 5.7 with performance_schema off.

## SQLite

SQLite is a file, not a server, so most rows above have nothing to read on it: no statement statistics, sessions,
waits, deadlocks, replicas or server counters, and Culprit says so in the UI and CLI rather than showing zeros. What it
reads, opening the file read-only (`docs/providers/sqlite.md`):

| Feature | SQLite |
|---|---|
| Statement statistics | ◐ SQLite keeps none; from the log the application's framework already writes when the connection has one (Rails, Django, Entity Framework Core, Phoenix/Ecto, JSON lines): runs, total, mean, max and spread per statement shape, over the period the log covers. No rows, reads or CPU |
| Plans | ✓ `EXPLAIN QUERY PLAN` for any statement: scans, searches by index, temporary b-trees, automatic indexes (aliases resolved to tables). ◐ No rows, costs or timings |
| Tables, columns, foreign keys | ✓ `sqlite_schema`, `pragma_table_xinfo`, `pragma_foreign_key_list` |
| Indexes | ✓ `pragma_index_list` / `pragma_index_xinfo`, direction and partial filters. – No usage counters: "never read" is never claimed |
| Row counts | ✓ counted in the `dbstat` pass that measures sizes, ◐ from `sqlite_stat1` when the file is too large to read whole; unknown without either, never `COUNT(*)` |
| Sizes | ✓ `dbstat` when the library has it and the file is small enough to read whole without holding up writers (4 GB in WAL mode, 128 MB with a rollback journal) |
| Settings | ✓ the ones that live in the file (`journal_mode`, `auto_vacuum`, page and free-page counts) |

