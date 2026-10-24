# PostgreSQL

[Engine parity](../parity.md)

Culprit uses PostgreSQL catalog and statistics views for PostgreSQL, including supported managed services such as Amazon RDS. Statement history comes from `pg_stat_statements` and Culprit's own saved scans.

## Connecting

Choose **PostgreSQL** and paste a PostgreSQL URL, libpq parameters, JDBC string or host/database address.

```sh
culprit conn add shop --env staging --url 'postgres://app@db.example.com:5432/shop'
```

The connection's TLS and password settings use the shared connection store. TimescaleDB uses this provider and is detected automatically; its additional behavior is documented on the [TimescaleDB page](timescale.md). YugabyteDB has a [separate native provider](yugabyte.md).

## Permissions and setup

`pg_read_all_stats` or `pg_monitor` provides broader visibility into recorded statements and monitoring views. A restricted login can see less; hidden or unreadable sources do not become zero counts.

`pg_stat_statements` must be loaded and its extension view installed in the database for statement counters. If the server already loads the module, installing the extension exposes existing statistics without another restart. `track_io_timing` and `pg_stat_statements.track_planning` control additional measurements. Hypothetical index testing requires `hypopg`.

Planning can invoke functions. Culprit restricts elevated planning to safely owned statements and refuses planning while other roles' reachable functions could borrow those privileges. Plans are obtained in read-only transactions that are rolled back.

## Workload and plans

| Feature | Coverage |
|---|---|
| Statement statistics | Cumulative `pg_stat_statements` calls, duration, rows and available read/write, planning and JIT measurements |
| Timeline and period comparison | Differences between Culprit's saved scans; the server does not retain Query Store-style runtime intervals |
| Captured plans | Safe generic plans; PostgreSQL 12–15 uses prepared statements and newer versions can use `EXPLAIN (GENERIC_PLAN)` |
| Estimated / measured plans | `EXPLAIN` for estimates; `EXPLAIN (ANALYZE, BUFFERS)` for an explicitly requested measured run |
| Last actual plan | Not retained by these statement counters; an `auto_explain` capture must come from the server log |
| Plan import | Text and JSON `EXPLAIN`, including psql output |
| Virtual indexes | `hypopg` evaluates candidate indexes in session memory without building persistent indexes |
| Runtime spread | Minimum, average, maximum and standard deviation per statement; aggregate spread does not identify the values used in slow executions |
| Statement families | Normalized texts and differences such as IN-list or VALUES lengths |

A changed query text receives its own identity. Statistics resets, eviction and scope changes constrain comparisons; the date of a Culprit scan is not the beginning of a cumulative counter's history.

## Live activity and waits

`pg_stat_activity` provides sessions and current activity; `pg_blocking_pids` supplies blocking relationships. Live wait sampling observes busy sessions once a second. `pg_stat_progress_*` views add progress for supported operations, while an estimated plan remains separate from measured live operator activity.

These sources do not retain a history of every wait. Recorded statement I/O timing covers timed data-file work; WAL-writer and commit work is not assumed to be part of the statement's execution time.

The database keeps a cumulative deadlock count. Import the relevant PostgreSQL log lines to reconstruct the participants and pattern. Cancel and terminate actions are available outside production, subject to the login's permissions.

## Schema and indexes

The catalog supplies tables, columns, constraints, statistics and index definitions. `pg_stat_user_indexes` provides usage; the collector records counter history where it can establish the reset period. PostgreSQL does not emit SQL Server-style missing-index requests: advice comes from captured plans, foreign keys, scans and the current index set.

Ordinary tables can use concurrent index builds or drops where PostgreSQL supports them. Invalid indexes are distinguished from indexes that are disabled or no longer maintained. Table-level statistics, autovacuum, dead tuples, HOT updates, transaction IDs, MultiXacts, sequence allocation and replication evidence retain their own scope and timestamps.

For managed hypertables and chunks, use the separate [TimescaleDB coverage](timescale.md); an empty parent heap is not hypertable storage.

## Limits and evidence

PostgreSQL heap and buffer measurements describe PostgreSQL storage. They are not reused for engines that share its SQL grammar but use different storage.

Statement counters are aggregates, not traces. A plan captured today uses the collector's current role and settings and need not match the application's earlier custom plan. Statement errors, deadlock graphs, historical parameters and `auto_explain` output require log or application evidence.

Per-day rates require enough dated history. A restart alone does not prove that PostgreSQL counters were reset, and absence claims require a week of counters. Shared [evidence rules](coverage.md) govern those conclusions.

### Operational evidence

Operational sources cover independent database/table MultiXact ages, shared archive failures, current-database subscription errors on PostgreSQL 15+, standby recovery cancellations, sequence allocation bounds and cleanup retained by a replication slot's `catalog_xmin`.

Counters retain their own reset clocks. New event findings require a comparable interval inside the requested period. Catalog-only retention does not establish retained application rows. Generic plans do not identify the plan used by historical requests, or prove that every listed partition ran.

## Verification

Coverage has been exercised on PostgreSQL 15–18, including Amazon RDS; saved fixtures include PostgreSQL 18.6. The [PostgreSQL fixture](../../dev/postgres/README.md) reproduces workload and schema problems. Parser fixtures and provider/embedding tests cover captured plans, missing measurements and offline replay.

The provider's declared sources are in [`culprit-postgres/src/sources.rs`](../../crates/culprit-postgres/src/sources.rs). Optional timing and extension features are tested independently of the base catalog.

Source references: [PostgreSQL vacuum thresholds and MultiXacts](https://www.postgresql.org/docs/18/routine-vacuuming.html), [sequence allocation](https://www.postgresql.org/docs/18/view-pg-sequences.html).
