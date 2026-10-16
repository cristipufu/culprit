# Oracle provider: design

Status: decided (see the end); in progress.

## Why, and what it can see for free

Oracle teams without the Diagnostics and Tuning Packs (and every Standard Edition 2 site, which can't have them) have
poor visibility: AWR, ASH and Enterprise Manager's performance pages are the paid part. Culprit can serve them from the
dynamic views that are free to query, plus its own history from one scan to the next, as it already does for
PostgreSQL and MySQL.

| Snapshot part | Oracle source (free) | Notes |
|---|---|---|
| Server, version, edition, start | `V$INSTANCE`, `V$DATABASE`, `V$VERSION`, `V$OPTION` | Edition decides advice: SE2 has no online index build, no partitioning |
| Workload (`QueryStat`) | `V$SQLSTATS` (per SQL_ID and plan), `V$SQL` for the text and schema | Times are microseconds; cumulative since each cursor was loaded, so a statement's window is its own (`FIRST_LOAD_TIME` / `LAST_ACTIVE_TIME`). Statements age out of the shared pool: unknown is not zero |
| Plans (`Plan`) | `V$SQL_PLAN` by SQL_ID, child and plan hash; `DBMS_XPLAN.DISPLAY_CURSOR` | Estimated rows and cost always; actual rows only with `STATISTICS_LEVEL = ALL` or the `gather_plan_statistics` hint (`V$SQL_PLAN_STATISTICS_ALL`) |
| Plan history | Culprit's previous scans | Oracle's own history (`DBA_HIST_*`) is AWR: paid |
| Index usage (`IndexStat.reads`) | `DBA_INDEX_USAGE` (12.2+) | **Sampled by default**: an index used rarely can show no use. "Never read" needs a long window and stays Low unless tracking is set to ALL |
| Tables, indexes, columns | `DBA_TABLES`, `DBA_TAB_STATISTICS` (`STALE_STATS`, `LAST_ANALYZED`), `DBA_INDEXES`, `DBA_IND_COLUMNS`, `DBA_SEGMENTS`, `DBA_TAB_MODIFICATIONS` | Modifications are flushed from memory periodically; Culprit never calls the flush procedure |
| Waits | `V$SYSTEM_EVENT`, `V$SYSTEM_WAIT_CLASS` (since start) | Rates need the start time (`V$INSTANCE.STARTUP_TIME`) and an hour of counting, like every engine |
| Sessions, blocking | `V$SESSION`, `V$LOCK`, `V$SESSION_WAIT` | ASH (`V$ACTIVE_SESSION_HISTORY`) is paid: never read without the licence switch below |
| Settings | `V$PARAMETER` | Changed with `ALTER SYSTEM … SCOPE=BOTH`, or the platform's control (RDS for Oracle, Autonomous) |
| Plan management | `DBA_SQL_PLAN_BASELINES`, SQL patches | Free; SQL profiles come from the Tuning Pack's advisor, so Culprit never proposes creating one |

Multitenant: a session in a PDB sees its own rows in `V$` views; scanning the CDB root is refused with a hint to connect
to the PDB (a snapshot describes one database).

## Licensing guard (tested from day one)

- Culprit reads no `DBA_HIST_*`, `V$ACTIVE_SESSION_HISTORY`, `DBMS_WORKLOAD_REPOSITORY` or SQL Tuning Advisor objects
  unless both are true: the connection is marked as licensed for the Diagnostics Pack (an explicit, per-connection
  setting), and `control_management_pack_access` includes `DIAGNOSTIC`.
- A test lists every statement the provider can send and fails on any paid object outside that gate.
- Advice never proposes a paid feature (SQL profiles, AWR reports) unless the connection is marked licensed.

## Fitting the architecture

- **A provider crate**, `crates/culprit-oracle`, behind the `oracle` cargo feature in `culprit-engine/src/registry.rs`,
  implementing `culprit_core::Provider`. Each source keeps its account in a `Coverage`: a missing privilege
  (`SELECT_CATALOG_ROLE`) is a missing capability with its GRANT, never a failed scan.
- **`Engine::Oracle`** in core, with its file in `crates/culprit-core/src/engines/` (`oracle.rs`: its `EngineSpec` and `Dialect`): quoting, `CREATE INDEX …
  ONLINE` where the edition has it, `DBMS_STATS.GATHER_TABLE_STATS`, `ALTER SYSTEM KILL SESSION 'sid,serial#'`. The
  dialect already carries that spelling for the three engines, the way `Platform` carries the cloud names. What remains
  of the 106 places that branch on the engine is engine-specific advice and thresholds, which each check states for
  the engines it opts into.
- **Checks opt in.** Each check's `engines` list gains Oracle only after its evidence rules hold for Oracle's numbers
  (for example, index usage being sampled). The first set: heavy queries, plan changes between scans, stale
  statistics, missing and unused indexes from plans and `DBA_INDEX_USAGE`, waits.
- **Plans.** `V$SQL_PLAN` rows become the shared `Plan` tree (operation and options map to `NodeKind`), so the plan
  viewer and the plan insights work unchanged once Oracle's operator names are mapped.
- **Safety on production.** Only `SELECT`s against dictionary and `V$` views, tagged `/* culprit */` (and filtered out
  of the workload). Oracle has no session statement timeout, so every call carries the driver's call timeout.
  On-demand estimated plans use `EXPLAIN PLAN`, which writes to the session's private `PLAN_TABLE`; plans of
  statements already in the cursor cache use `DISPLAY_CURSOR` and write nothing.
- **Demo database.** `dev/oracle`, Oracle Database Free in Docker (`gvenzl/oracle-free`), with planted problems like the
  other demos: a missing index, a stale table, a plan change, an unused index.

## Decisions (2026-10-15)

1. **Driver:** users install Oracle Instant Client. The provider loads it at runtime (the `oracle` crate, ODPI-C
   compiled in), so builds and every non-Oracle connection are unaffected; when it's missing, connecting says what to
   install and where Culprit looks (`CULPRIT_ORACLE_CLIENT`, then the platform's usual library paths).
2. **Shipping:** in releases from the first working version, marked preview, with the checks that have been verified
   against Oracle's numbers only.
3. **Licensing:** free views only. No AWR, ASH or Tuning Pack object is read, and the test that lists every statement
   the provider sends enforces it.
