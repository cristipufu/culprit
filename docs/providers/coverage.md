# Evidence and provider coverage

Culprit analyzes the evidence its collectors can read. A scan is not an exhaustive database certification.
Each report includes `assessments`: a check can have findings, be assessed without a finding, have insufficient
evidence, be unavailable, or be inapplicable. Supported findings can coexist with incomplete evidence. The app's
Check coverage panel and CLI JSON retain the reasons. Old reports without these assessments need a fresh analysis.

Query and plan recommendations preserve result semantics, including NULLs, ordering and pagination. A changed
operator is not sufficient verification: compare equivalent results, reads and duration under representative data.
An inexpensive run can still expose a scaling risk; its structural observation stays available without implying
an urgent index migration today.

| Provider | Additional evidence | Boundaries |
| --- | --- | --- |
| SQL Server / Azure SQL | Current database log occupancy and reuse reason; writable filegroup allocation and configured growth; shared tempdb allocation split into user objects, internal objects and version store; exact identity/sequence numeric bounds | Filegroup capacity is aggregated across files. Shared tempdb is context, not attributed to the selected database. Configured growth does not prove filesystem headroom. Reserved sequence values do not forecast remaining requests. Unreadable DMVs remain unavailable. |
| PostgreSQL | Independent database/table MultiXact ages; shared archive failures; current-database subscription errors on 15+ and standby recovery cancellations; sequence allocation bounds; cleanup retained by slot catalog_xmin | Counters retain their own reset clocks. New event findings require a comparable interval inside the requested period. Catalog-only retention does not establish retained application rows. Generic plans do not identify the plan used by historical requests, or prove that every listed partition ran. |
| MySQL / MariaDB | InnoDB redo buffer waits; auto-increment bounds; metadata blocking involving exclusive locks; all MariaDB replication channels; Galera readiness and flow-control history | Digests are aggregates, not request traces. Shared redo counts do not measure this database's delay. Other metadata-lock modes and queue effects can remain unresolved; missing instrumentation is explicit. Transaction age alone does not prove an acquired read view. Galera donor/desynced state alone is not an outage; flow control retains its own source period. |
| SQLite | Per-index sqlite_stat1 coverage; WAL file allocation from filesystem metadata; recognized structured SQL errors, even without durations; repeated shapes correlated by explicit request_id | No checkpoint is run and the WAL is never deleted. WAL allocation does not measure uncheckpointed work. Missing durations stay out of performance totals. Raw error messages, literal values and request IDs are not retained. Repetition does not prove an N+1 bug. External logs with shared table names cannot prove database attribution. |

Current allocation, ages and queues describe scan time. Historical totals retain their dates and do not become
selected-window counters. A counter comparison needs matching identity, scope and reset, monotonically increasing
counts, and scans in chronological order. Without that evidence, an observation stays historical context.

Index availability and maintenance are different facts. MySQL invisible and MariaDB ignored indexes are still
maintained. PostgreSQL invalid indexes may be maintained when `indisready` is true. SQL Server disabled indexes
are not maintained. Failed schema reads cannot justify an absent-index recommendation.

Backup recoverability, corruption, filesystem capacity, application transaction boundaries and distributed request
traces require evidence beyond a statistics scan. Culprit does not report these areas healthy from their absence.
SQLite additionally cannot inspect connection-local pragmas or application-loaded modules through its own connection.
An actual plan or explicitly approved measured run is needed for operator timing, measured row estimates and trigger
work; trigger timing can overlap its modifying operator and must not be blindly added to the operator tree.

Source references: [SQL Server file-space DMV](https://learn.microsoft.com/en-us/sql/relational-databases/system-dynamic-management-objects/sys-dm-db-file-space-usage-transact-sql),
[PostgreSQL vacuum thresholds and MultiXacts](https://www.postgresql.org/docs/18/routine-vacuuming.html),
[PostgreSQL sequence allocation](https://www.postgresql.org/docs/18/view-pg-sequences.html),
[MySQL metadata-lock view](https://dev.mysql.com/doc/refman/8.4/en/sys-schema-table-lock-waits.html),
[MariaDB replication channels](https://mariadb.com/docs/server/reference/sql-statements/administrative-sql-statements/show/show-replica-status),
[Galera status periods](https://mariadb.com/docs/galera-cluster/reference/galera-cluster-status-variables),
and [SQLite error semantics](https://www.sqlite.org/rescode.html).
