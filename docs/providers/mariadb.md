# MariaDB

[Engine parity](../parity.md)

Culprit detects MariaDB through the MySQL provider and uses MariaDB-specific plans, statistics, lock sources and index syntax. Its counters and capability limits are kept separate from [MySQL](mysql.md).

## Connecting

Choose **MySQL / MariaDB** and supply the server's MySQL-compatible connection string. The probe reads the server's version and identifies it as MariaDB for labels, SQL generation and collection.

```sh
culprit conn add shop --env staging --url 'mysql://app@db.example.com:3306/shop'
```

TLS, credential storage, saved connections and agent permissions use the shared connection workflow.

In JSON, MariaDB uses `engine = "mysql"`; `latest_scan.product` / `server.product` identifies the server, and parsed plans carry `props.flavor = "MariaDB"`. Use those product facts when interpreting the shared engine identifier.

## Permissions and setup

Monitoring needs access to the relevant process, catalog and statistics views, commonly `PROCESS` plus `SELECT` on the monitoring sources. `performance_schema` can be off by default. Statement-history consumers must be enabled before they can supply captured SQL.

Without performance-schema digests, Culprit can read the slow query log table and, when enabled, user statistics. The setup guidance names `userstat`, `slow_query_log`, `log_output` and `long_query_time` rather than assuming a restart is the only way to record statements. It shows the settings for a person to apply; a scan does not change them.

When nothing is recorded, the `performance-schema` entry in `server.missing[]` presents the no-restart `SET GLOBAL …` guidance first. Capability `slow-log` means the log table is readable; it does not establish complete application coverage.

## Workload and plans

| Feature | Coverage |
|---|---|
| Statement statistics | Performance-schema digests, or individual runs read from the slow query log table |
| Timeline and period comparison | Differences between saved digest scans; the slow log's own timestamps and retained period when it supplies the workload |
| Captured statements | Statement-history consumers and slow-log samples matched to statement identities |
| Estimated / measured plans | MariaDB `EXPLAIN FORMAT=JSON` and `ANALYZE FORMAT=JSON`; measurements use a read-only transaction |
| Plan import | MariaDB EXPLAIN / ANALYZE JSON with per-loop rows, loops, filtering and available timing/storage facts |
| Last actual plan | Not retained by the collected statement counters; live SHOW ANALYZE is a different observation |
| Runtime spread | Digest minimum, average and maximum; percentiles and histograms computed from the individual slow-log runs when present |
| Parameter sensitivity | Logged-tail evidence where the source supplies it; persistent column-statistics advice uses MariaDB's syntax |
| Statement families | Digests or fingerprints group literal variants |

Before MariaDB 11.0, plan rows can be used as a cost proxy when the plan supplies no native cost. Those values are not calibrated PostgreSQL or MySQL optimizer costs.

Older MariaDB JSON plans can contain repeated `table` members instead of a `nested_loop` array. The parser retains each joined table and accounts for buffered reads rather than assuming one MySQL plan layout.

`workload.source` distinguishes `"performance_schema"` from `"slow query log"`. Slow-log workloads are windowed, name the `long_query_time` threshold, and use fingerprint query IDs stable across scans. Their timeline and changes source is `"slow-log"`; two-period comparisons use the log's own timestamps. A missing period is not zero activity.

CLI `explain --query-id` and MCP `query_plan` can return the plan captured by the scan when normalized SQL cannot be planned again. Measured plans carry available per-operator time, logical page reads and spill evidence.

## Live activity and waits

Live uses MariaDB's process list, transaction metadata, `INNODB_LOCK_WAITS` and `INNODB_LOCKS`. Process-list time can have millisecond precision; ALTER/LOAD operation state and progress remain distinct from per-operator progress.

MariaDB 10.x can report transaction ID zero for transactions that have not written. Waiting sessions are matched through their requested lock; transaction ID zero alone does not establish a blocking relationship.

MariaDB 10.9+ can expose a running plan through `SHOW ANALYZE`; older versions report that this source is unavailable. Cancel (`KILL QUERY`) and terminate (`KILL`) are available outside production, subject to permissions.

With performance schema, live wait samples compare its counters and can fall back to process-list observations. Without it, process-list sampling and InnoDB transaction evidence show current waits. Slow-log lock time belongs only to those logged statements, not to a complete server wait mix. InnoDB's latest deadlock is a retained example, not an exhaustive incident history.

## Schema and indexes

Catalog views supply tables, columns, constraints, indexes and MariaDB persistent column statistics. Index usage comes from performance schema or, where enabled, userstat `INDEX_STATISTICS`. Userstat's enable time is not recorded by the server; an unknown period does not establish that an index was unused for a week.

MariaDB 10.6+ can mark an eligible index `IGNORED`. It remains maintained and `NOT IGNORED` restores optimizer use. Older releases cannot hide indexes; unknown versions do not establish support. Advice does not substitute MySQL's `INVISIBLE` syntax.

Column-statistics recommendations use `ANALYZE TABLE … PERSISTENT FOR COLUMNS` where appropriate. Scan advice uses measured rows examined and returned when available, rather than assuming an unindexed filter's estimated selectivity is accurate.

## Limits and evidence

MariaDB's server-wide status counters are not all attributable to one database. Shared redo, metadata-lock visibility, old transactions, replication channels and Galera flow-control observations keep their own scope and periods.

Slow-log entries are individual logged executions; digest and userstat counters are aggregates with independent retention and reset limits. A table-lock duration can overlap other measurements and is not added blindly to them. No duration or missing source is represented as zero just to make views line up.

Donor or desynced Galera state alone is not an outage. A complete request trace, transaction boundary or backup guarantee needs evidence beyond these sources. The shared [evidence rules](coverage.md) apply to every finding.

### Operational evidence

Operational sources cover InnoDB redo buffer waits, auto-increment bounds, metadata blocking involving exclusive locks, all MariaDB replication channels, and Galera readiness and flow-control history.

Digests are aggregates, not request traces. Shared redo counts do not measure this database's delay. Other metadata-lock modes and queue effects can remain unresolved; missing instrumentation is explicit. Transaction age alone does not prove an acquired read view. Galera donor/desynced state alone is not an outage; flow control retains its own source period.

## Verification

Verified environments include MariaDB 10.5.29, 10.6.28, 10.11 and 11.8. Cases cover performance schema on and off, slow-log fallback, user statistics, blocking chains, deadlocks, plans and live progress.

The [MySQL/MariaDB fixture](../../dev/mysql/README.md) supplies the schema and workload. Saved MariaDB reports and integration tests exercise both the normal and fallback paths. MariaDB 10.4 and earlier have not been verified. The shared provider declares its sources in [`culprit-mysql/src/sources.rs`](../../crates/culprit-mysql/src/sources.rs).

Source references: [MariaDB replication channels](https://mariadb.com/docs/server/reference/sql-statements/administrative-sql-statements/show/show-replica-status), [Galera status periods](https://mariadb.com/docs/galera-cluster/reference/galera-cluster-status-variables).
