# Engine parity

Culprit uses the same workspace, findings, plans and agent tools across databases. The available evidence depends on
what each database records. Choose a provider below for its setup, sources, supported operations, limits and verified
versions.

## Providers

| Database | Connection selection | Main evidence |
|---|---|---|
| [SQL Server](providers/mssql.md) | SQL Server, including Azure SQL Database | Query Store, plan cache, DMVs and Extended Events |
| [PostgreSQL](providers/postgres.md) | PostgreSQL, including Amazon RDS | `pg_stat_statements`, catalog/statistics views and EXPLAIN |
| [TimescaleDB](providers/timescale.md) | PostgreSQL; extension detected automatically | PostgreSQL sources plus logical hypertable sizes and managed/compressed chunks |
| [YugabyteDB](providers/yugabyte.md) | YugabyteDB | Native YSQL/DocDB counters, DIST plans, sharding and scoped activity sources |
| [MySQL](providers/mysql.md) | MySQL / MariaDB | `performance_schema`, `sys`, EXPLAIN and the slow query log |
| [MariaDB](providers/mariadb.md) | MySQL / MariaDB; product detected automatically | MariaDB plans, performance schema, slow log, user statistics and lock views |
| [SQLite](providers/sqlite.md) | SQLite file | Read-only file/catalog inspection, EXPLAIN QUERY PLAN and optional application query logs |

Every provider page follows the same structure:

1. Connecting
2. Permissions and setup
3. Workload and plans
4. Live activity and waits
5. Schema and indexes
6. Limits and evidence
7. Verification

## Feature overview

**✓** implemented · **◐** limited or supplied by a different source · **—** not supplied by the provider.
Availability still depends on the server version, enabled sources and login permissions. The provider pages explain
those conditions and the measurement period behind each result.

| Feature | SQL Server | PostgreSQL | TimescaleDB | YugabyteDB | MySQL | MariaDB | SQLite |
|---|---|---|---|---|---|---|---|
| Statement workload | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ query log |
| Timelines and period comparison | ✓ intervals | ◐ scans | ◐ scans | ◐ matching scans | ◐ scans / slow log | ◐ scans / slow log | — |
| Estimated plans / access paths | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ access paths only |
| Explicitly measured execution plans | ✓ | ✓ | ✓ | ✓ DIST | ◐ version-dependent | ✓ ANALYZE | — |
| Retained last actual execution plan | ◐ when enabled | — | — | — | — | — | — |
| Live sessions and blocking | ✓ | ✓ | ✓ | ◐ native scoped evidence | ✓ | ✓ | — |
| Live statement / operation progress | ✓ operator profiles | ◐ supported operations | ◐ PostgreSQL operations | ◐ supported native operations | ◐ plan / stages | ◐ version-dependent | — |
| Recorded waits / activity | ✓ Query Store waits | ◐ timed statement I/O | ◐ PostgreSQL sources | ◐ weighted ASH observations | ◐ counters / lock time | ◐ counters / logged lock time | — |
| Live wait sample | ✓ counter deltas | ✓ sessions | ✓ sessions | ✓ sessions | ◐ counters / sessions | ◐ counters / sessions | — |
| Deadlock evidence | ✓ retained events | ◐ counter / imported log | ◐ counter / imported log | — | ◐ latest report | ◐ latest report | — |
| Index usage | ✓ | ✓ | ◐ ordinary tables | ◐ complete cluster counters | ✓ | ◐ performance schema / userstat | — |
| Health and storage | ✓ | ✓ | ◐ logical storage and PostgreSQL sources | ◐ DocDB / shared nodes | ✓ | ✓ | ◐ file evidence |

Saved scans, offline analysis, plan comparison, schema browsing, migration linting, exports and agent permissions share
one workflow. Each provider's page defines what its underlying measurements and execution capabilities can establish.

## Evidence and missing data

Missing or failed sources are shown with a reason. They do not produce zero measurements or a healthy assessment.
Counters, sampled activity, retained events and current allocation keep their own dates, scope and units.

Rates need enough dated history. Claims that an index has never been used require at least a week of relevant counters.
Comparisons require matching database identity, scope, contributors and reset history. The shared
[evidence rules](providers/coverage.md) explain how those conditions apply to findings.

<a id="verified-on"></a>
<a id="versions"></a>

## Verification

Each provider page records its verified versions, reproducible database or file fixture, and relevant source
implementation. Older-version behavior and product differences are documented there alongside the feature they
affect. A successful connection does not establish access to every optional source.

For shared commands and workflows, see the [guide](guide.md). For collection and analysis boundaries, see the
[architecture](ARCHITECTURE.md).
