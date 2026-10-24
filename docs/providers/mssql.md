# SQL Server

[Engine parity](../parity.md)

Culprit uses the SQL Server provider for SQL Server and Azure SQL Database. Query Store supplies time-windowed workload history; the plan cache and DMVs supply the evidence available when Query Store is not usable.

## Connecting

Choose **SQL Server** and paste an ADO.NET, JDBC or `mssql://` connection string. Azure SQL connections can use Microsoft Entra ID through the existing Azure CLI sign-in or Culprit's device-code flow.

```sh
culprit conn add shop --env staging --url 'Server=tcp:db.example.com,1433;Database=shop;User ID=app'
```

Passwords stay in the OS keychain. Agent permissions and production confirmations apply separately to each saved connection.

## Permissions and setup

Culprit checks the login's permissions and Query Store state before collection. SQL Server 2022 monitoring views require `VIEW SERVER PERFORMANCE STATE`; older SQL Server versions use `VIEW SERVER STATE`. Azure SQL Database uses database-scoped monitoring permissions such as `VIEW DATABASE STATE`. Individual sources can require additional access.

Query Store must be enabled and readable for interval history, recorded plans and Query Store wait statistics. The plan cache provides a fallback with a different period and retention. Missing permissions or disabled sources produce an explanation and the relevant grant or setting, rather than an empty healthy result.

## Workload and plans

| Feature | Coverage |
|---|---|
| Statement statistics | Query Store executions and runtime intervals; plan-cache counters when Query Store is unavailable |
| Timeline and period comparison | Query Store intervals support selected windows, trends and two-period comparisons; plan-cache comparisons use Culprit's own scans |
| Captured plans | Stored Query Store or cached compilation plans, with plan identities and compiled parameter values where recorded |
| Estimated / measured plans | Estimated Showplan does not execute the statement; measured plans use `STATISTICS XML`, with measured writes rolled back |
| Last actual plan | Retained last actual plans on SQL Server 2019+ when `LAST_QUERY_PLAN_STATS` is available |
| Plan import | SSMS `.sqlplan` and Showplan XML |
| Runtime spread | Recorded minimum, average, maximum and standard deviation per plan; compiled parameter values are compilation evidence, not a trace of every execution |
| Plan and query regressions | Plan history and timed runtime evidence; plans with different retention periods are not treated as interchangeable |
| Tuning recommendations | Query Store plan-forcing recommendations, forced-plan state and failures, where readable |
| Statement families | Query hashes, parameter declarations and the captured statement texts |

Planning, compile history, execution time, CPU, waits, rows, reads and log bytes retain their source's own meaning and dates. Compilation entry totals are not silently assigned to a selected runtime window.

## Live activity and waits

Live shows current sessions, running statements, idle transactions and blocking chains. Query Store attributes recorded wait categories to statements; live wait samples compare server wait counters over the sample interval.

When query profiling is available, live progress shows measured operator activity. SQL Server offers **terminate** (`KILL`), not a separate cancel operation; Culprit exposes session actions outside production only.

Deadlock history comes from the readable Extended Events history, such as `system_health`. A missing or truncated event source does not establish that no deadlocks happened.

## Schema and indexes

Catalog and DMV sources provide tables, columns, constraints, statistics, index definitions and usage. Missing-index requests come from the optimizer's own DMVs and are checked against the existing index set.

The advisor combines those requests with workload and plan evidence, unindexed foreign keys, and duplicate, overlapping or unused indexes. Disabling an eligible index keeps its definition and stops its maintenance; `ALTER INDEX … REBUILD` restores it. Clustered, unique and constraint-backed indexes are not treated as disposable unused indexes.

Schema drift and migration linting use the saved catalog, actual table sizes and captured workload. The proposed SQL remains a suggestion for review.

## Limits and evidence

Query Store and the plan cache are aggregate histories, not application request traces. Cache eviction, resets, retention, permissions and the selected interval limit what can be compared. A compilation plan does not measure the work of the execution being investigated.

Host CPU or filesystem capacity is shown only when the relevant source supplies it. Backup recoverability, database corruption and application transaction boundaries cannot be certified from a successful statistics scan. Cloud limits retain their measured scope and period.

Rates require sufficient history, and an unused-index verdict requires at least a week of dated counters. See the shared [evidence rules](coverage.md).

### Operational evidence

Current database log occupancy and reuse reasons, writable filegroup allocation and configured growth, shared tempdb allocation by user objects, internal objects and version store, and exact identity/sequence numeric bounds are collected where readable.

Filegroup capacity is aggregated across files. Shared tempdb is context, not attributed to the selected database. Configured growth does not prove filesystem headroom. Reserved sequence values do not forecast remaining requests. Unreadable DMVs remain unavailable.

## Verification

The saved examples cover SQL Server 2022 and Azure SQL Database. The reproducible [SQL Server fixture](../../dev/mssql/README.md) plants workload, index, blocking and deadlock problems; saved snapshots exercise offline replay, plan analysis, views and agent output.

The implementation declares its sources in [`culprit-mssql/src/sources.rs`](../../crates/culprit-mssql/src/sources.rs). Version-, edition- and permission-dependent measurements remain unavailable when the server does not supply them.

Source references: [SQL Server file-space DMV](https://learn.microsoft.com/en-us/sql/relational-databases/system-dynamic-management-objects/sys-dm-db-file-space-usage-transact-sql).
