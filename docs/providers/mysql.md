# MySQL

[Engine parity](../parity.md)

Culprit uses the MySQL provider for MySQL. `performance_schema` supplies statement and wait counters; a readable slow query log table provides an alternative workload source. MariaDB uses the same transport with its own [coverage page](mariadb.md).

## Connecting

Choose **MySQL / MariaDB** and use a MySQL URL, JDBC/.NET connection string, or host/database address. The server probe identifies the product and version.

```sh
culprit conn add shop --env staging --url 'mysql://app@db.example.com:3306/shop'
```

Configure TLS and credentials on the saved connection. Managed services can restrict individual statistics or administrative views even when the connection succeeds.

## Permissions and setup

Monitoring commonly needs `PROCESS` and read access to the relevant `performance_schema` and `sys` views. Statement-history consumers must be enabled for the history used to recover captured SQL. Culprit reports the source and grant or configuration it cannot use.

When `performance_schema` is unavailable, a readable slow query log table (`log_output=TABLE`) can provide workload samples without a performance-schema restart. Only logged runs belong to that workload; its coverage is not every application request. Changes to server instrumentation are shown for a person to apply.

## Workload and plans

| Feature | Coverage |
|---|---|
| Statement statistics | `performance_schema` digests, or grouped runs from the slow query log table |
| Timeline and period comparison | Differences between saved digest scans; individual timestamps for runs read from the slow log |
| Captured statements | Digest sample text on MySQL 8.0.3+; statement-history or slow-log samples when a digest has no usable sample |
| Estimated / measured plans | `EXPLAIN FORMAT=JSON`; `EXPLAIN ANALYZE` tree output on MySQL 8.0.18+ in a read-only transaction |
| Last actual plan | Not retained as a past-run plan by the collected statement counters |
| Plan import | JSON EXPLAIN variants and tree output |
| Runtime spread | Digest extrema and, where available, runtime histograms and percentile bucket bounds; slow-log runs have their own observed distribution |
| Parameter sensitivity | Supported histogram or logged-tail evidence, without inventing values for historical executions |
| Statement families | Digests and fingerprints merge literal variants |

A normalized `WHERE id = ?` text is not itself executable parameter evidence. A captured plan remains available when the provider cannot safely plan that normalized text again.

## Live activity and waits

Live combines `PROCESSLIST`, transaction metadata and available lock-wait views. MySQL 8 uses `performance_schema.data_lock_waits`; older versions use `INNODB_LOCK_WAITS`. Statement-history consumers can identify a blocking transaction's earlier statement when the current process-list text is empty.

`EXPLAIN FOR CONNECTION` and stage instrumentation provide the current plan and operation progress where supported. Cancel (`KILL QUERY`) and terminate (`KILL`) actions are exposed outside production.

Historical waits are performance-schema totals, with lock timing attributable to statements where supplied. Live samples normally compare instrument counters and fall back to process-list sampling when those instruments provide nothing. InnoDB retains the latest detected deadlock, not a complete deadlock history; imported reports explain the captured pattern.

## Schema and indexes

`information_schema`, `performance_schema` and `sys` provide the catalog, table activity, index usage and available column histograms. Missing-index candidates are derived from workload, plans and foreign keys rather than a native missing-index DMV.

MySQL 8.0+ can make an eligible index `INVISIBLE` before removal; it remains maintained. MySQL 5.7 cannot hide it, so advice preserves the definition before suggesting removal. Unknown versions do not establish index-hiding support. Foreign-key backing indexes and unique keys are protected from ordinary cleanup advice.

The schema view, migration linter and drift use these same collected definitions and measurements.

## Limits and evidence

Digest, histogram, table and index counters can have different reset histories. Histograms count their recorded period, not an arbitrary selected window. A missing histogram can mean an unsupported version, disabled instrumentation, permissions or collection failure; it is not proof of one particular cause.

Culprit does not attribute shared redo or host observations to this database's query delays without matching evidence. Transaction age does not establish when a consistent read view was acquired. Missing lock instrumentation leaves unresolved blocking rather than a clean bill of health.

A slow-log workload covers only logged runs and its own timestamps. Rates and unused-index conclusions follow the shared [evidence rules](coverage.md).

### Operational evidence

InnoDB redo buffer waits, auto-increment bounds and metadata blocking involving exclusive locks retain their source scope and measurement period.

Digests are aggregates, not request traces. Other metadata-lock modes and queue effects can remain unresolved; missing instrumentation is explicit. MariaDB replication and Galera evidence are documented on the [MariaDB page](mariadb.md#limits-and-evidence).

## Verification

Verified versions include MySQL 5.7.44 and 8.4; the saved fixture records MySQL 8.4.11. The [MySQL fixture](../../dev/mysql/README.md) provides repeatable schema, workload, blocking and deadlock cases.

| Version or source | Behavior |
|---|---|
| MySQL 5.7 | No `EXPLAIN ANALYZE`, digest sample text, invisible indexes or MySQL 8 lock-wait view; the provider uses the available history and lock sources instead |
| MySQL 8.0.19+ | Runtime histograms can be available when the instrumentation and permissions allow them |
| Older MySQL 5.7 TLS builds | A server offering no shared cipher suite is reported as such; only TLS modes that permit plaintext fallback can fall back |

MySQL 5.6 and earlier, Percona Server and MySQL 5.7 with `performance_schema` off have not been verified. Source declarations live in [`culprit-mysql/src/sources.rs`](../../crates/culprit-mysql/src/sources.rs).

Source references: [MySQL metadata-lock view](https://dev.mysql.com/doc/refman/8.4/en/sys-schema-table-lock-waits.html).
