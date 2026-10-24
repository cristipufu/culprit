# YugabyteDB

Culprit has a dedicated `yugabyte` provider for YSQL over DocDB. PostgreSQL supplies the wire transport and SQL grammar;
its heap, buffer-cache and vacuum collectors are not used. This support is in the development tree after 0.4.4.

Choose **YugabyteDB** before pasting a PostgreSQL URL, or use a native URL:

```text
yugabyte://app@db.example.com:5433/shop
```

The CLI accepts `--engine yugabyte` (`ysql` and `yugabytedb` are aliases). The defaults are port 5433 and user/database
`yugabyte`. Passwords use the existing OS-keychain flow. Remote TLS verifies the server unless the connection explicitly
requests another mode. Azure Entra and RDS IAM bearer tokens are refused for this provider.
The PostgreSQL provider recognizes a YugabyteDB server before calling storage APIs and asks you to select YugabyteDB,
so a PostgreSQL URL cannot silently enable heap-oriented collection on DocDB.

Verified against **YugabyteDB 2026.1.2.0-b137**, whose YSQL server identifies as **15.12-YB-2026.1.2.0-b0**. Older releases
are not claimed as verified; optional views and functions that are absent or denied become coverage notes.

## Measurements and scope

| Area | Current implementation |
|---|---|
| Connection test | Version, database identity, clock and feature availability; no `pg_database_size` call |
| Workload | Database-filtered `pg_stat_statements`, aggregated by query identity; top queries retain totals for the complete source |
| Native costs | DocDB RPCs, storage operations/entries, timings, retries and LSM traversal counters, only for metric families enabled on every contributing node |
| Captured plans | Safe generic/estimated YSQL plans; elevated logins plan their own recorded text, including their copy on another node when global views are configured |
| Analyze | `EXPLAIN (ANALYZE, DIST, VERBOSE, FORMAT JSON)`; measured writes roll back |
| Plan files | Native JSON with DIST/storage markers is detected as YugabyteDB; saved scans reparse with the native reader |
| Catalog | Tables, row estimates, columns, defaults, foreign keys, index definitions, hash keys, tablet counts and colocation |
| Deferred constraints | CHECK and foreign-key constraints added NOT VALID; findings explain unvalidated existing rows and suggest version-appropriate validation, without PostgreSQL planner or locking promises |
| Sizes | Native table/index size APIs, separately from the cheap probe; primary-key storage is part of the table; colocated objects are not charged their shared tablegroup's allocation |
| Index advice | Ordered candidate keys use explicit ASC/DESC; HASH groups differ from range keys in redundancy, lookup coverage and drift; active backfill must finish before repairing an invalid index |
| Index usage | Complete global index counters when available; no unused-index verdict from a local zero or unknown reset date |
| Live | Current-database sessions and native `yb_lock_status` blocking evidence; node-qualified identities include backend start time |
| Live samples / progress | Bounded session sampling with native RPC/storage categories; scoped current activity and optional estimated plans; supported CREATE INDEX / ANALYZE phases without guessed percentages |
| Session actions | Non-production only; local node, database and backend-start guards; Connection Manager worker backends are refused |

Workload retention streams at most 8 MiB of statement text and counters, without whole-row latency-histogram expansion.
An oversized response retains a contiguous prefix and records partial coverage; no retained totals means unknown
workload rather than a zero. Completion and late errors are still read after retention stops.

The plan-capture phase has one 20-second budget and retains at most 8 MiB of native plan/statement text. Remaining
statements can be planned on demand. Operator DIST counters are per-loop averages; normalized row counts multiply by
executions once. Storage entries can include index and table reads of the same SQL row. Native timings may overlap;
they are not PostgreSQL page counts, CPU utilization or exclusive shares of statement time.

Saved-scan plan context uses the catalog's logical YSQL row estimates, without heap-page inference. Native optimizer
costs have no calibrated materiality threshold in Culprit: a large or small cost number alone does not establish
expensive or cheap work. Measured operator work and elapsed time remain separate from those estimates.

Without `pg_read_all_stats`, statement totals are filtered to the current role before aggregation and labeled with that role scope. Hidden queries never become an invented query group.

Ordinary statistics are **node-local**. The scope is visible in the workload source and coverage notes. Culprit does not
enable or configure the optional [cluster-wide database views](https://docs.yugabyte.com/stable/launch-and-manage/monitor-and-alert/cluster-wide-db-views/)
preview during a scan. When an administrator has enabled it and granted access, the provider reads the existing global
views and verifies the node inventory. Warnings, skipped contributors, membership changes or independent resets withdraw
complete comparisons. Per-node reset timestamps retain microsecond precision. Culprit's scans compare only matching
node sets and reset generations.

The default global views do not include statement reset metadata or metric settings. An administrator can configure
corresponding global views outside a Culprit scan:

```sql
SELECT yb_create_global_view('public', 'pg_stat_statements_info');
SELECT yb_create_global_view('public', 'pg_settings');
```

Culprit accepts these custom views only through `yb_global_views_server` and with a superuser owner. Global views need
`pg_read_all_stats`; availability and permissions are checked by reading them. Native cost totals describe recorded
work; enabling collection later does not reconstruct older work.

YugabyteDB can record SQL generated inside its size functions and global-view implementation without the caller's
query tag. Those monitoring statements can therefore appear in statement totals; Culprit's application-query checks
exclude catalog and monitoring text. Culprit does not change the cluster's statement-tracking configuration.

A global database reset date can be NULL. That means unknown, even on a long-running node. Index absence requires at
least a week since every contributing node's proven database reset; differing known epochs can bound absence but do
not supply a common denominator for positive-rate calculations.

## Limits

PostgreSQL heap pages, buffer ratios, dead tuples, HOT updates, autovacuum, XID/MultiXact wraparound and heap-oriented GUC
advice are unavailable by design. Database size remains unknown. Configuration suggestions are restricted to supported
YSQL database settings; other changes use YugabyteDB administration rather than PostgreSQL `ALTER SYSTEM` guesses.

The current provider does not yet collect native ASH history, saved `yb_pg_stat_plans` history, complete native operation progress,
server resource pressure or distributed error/deadlock history. These are implementation gaps;
PostgreSQL placeholders are not substituted. Current session/lock collection is limited to 1,000 sessions and 2,000
waits and labels truncated or node-local visibility incomplete. Live lowers the lock metadata reader's session-local
minimum transaction age to include young waiters; native transaction/per-tablet caps still limit its evidence. A no-blocking result is not a whole-cluster assertion
when visibility is incomplete. Remote sessions can be observed through global views, but actions require reconnecting
to their node. Logical Connection Manager clients are not identified by reusable physical backend PIDs.

Constraint metadata retains at most 10,000 rows and 8 MiB; truncation records partial coverage. On the verified release,
both CHECK and foreign-key [deferred validation](https://docs.yugabyte.com/stable/yugabyte-voyager/reference/non-superuser/)
were exercised against existing invalid rows and collected again after successful validation. Validation can scan
distributed data and acquire locks; review its behavior on the deployed YugabyteDB version before applying the fix.

Sequence capacity needs a native allocation-state source: `pg_sequences.last_value` returned NULL even for initialized
sequences on the verified release. The native implementation of
[`pg_sequence_last_value`](https://github.com/yugabyte/yugabyte-db/blob/master/src/postgres/src/backend/commands/sequence.c)
returns NULL instead of reading DocDB sequence state. Session-local `currval` and `lastval` cannot supply a cluster allocation watermark,
and a scan must not call `nextval` to discover one. No remaining-allocation estimate is inferred from those NULLs.

A plan without native storage markers is structurally indistinguishable from PostgreSQL JSON. Plans captured through
the YugabyteDB provider retain the correct engine; a plain external PostgreSQL-shaped plan needs native DIST output
for automatic detection. Approximate table DDL preserves known primary-key sharding and colocation, but tablespaces,
placement, tablegroups and range split boundaries require a native schema export before recreation.

The live global-view verification so far uses a one-node cluster. Multi-node failure/reset and Connection Manager
scenarios need additional verification before claiming that deployment coverage.

See [dev/yugabyte](../../dev/yugabyte/README.md) for the disposable database, seed workload and opt-in checks.

## Verified UI

These screenshots show the development UI reading the live fixture described above. They are not screenshots of the
published 0.4.4 build.

![YSQL workload scope and dashboard](../screenshots/yugabyte-dashboard.png)
![Recorded native DocDB work per statement run](../screenshots/yugabyte-queries.png)
![Native hash keys and unavailable local index-usage counters](../screenshots/yugabyte-schema.png)
