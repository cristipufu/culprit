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
| Where time goes (history) | ✓ Query Store waits per query | ◐ per-statement IO/WAL time | ◐ performance_schema totals; lock time per digest | ◐ the same with performance_schema; without it, lock time per statement from the slow log |
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
containers, seeded with `dev/mysql/seed.sql` and its workload).
