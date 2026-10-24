# YugabyteDB development database

Disposable YSQL/DocDB fixture for dedicated provider development. Verified server: **2026.1.2.0-b137**, reporting PostgreSQL compatibility **15.12**. All seed data is synthetic.

```sh
docker run --rm -d --name culprit-yugabyte-provider --cpus 3 --memory 6g \
  -p 127.0.0.1:55434:5433 \
  yugabytedb/yugabyte@sha256:e604d9e273a1fc3fccaf6ecbfe6ac6e3b93d2b4f06a075c461e3d73676d183fc \
  bin/yugabyted start --background=false --callhome=false \
  --tserver_flags=memory_limit_hard_bytes=2147483648 \
  --master_flags=memory_limit_hard_bytes=536870912
docker exec culprit-yugabyte-provider bin/ysqlsh -h culprit-yugabyte-provider -X -c 'CREATE DATABASE culprit_yugabyte;'
docker exec -i culprit-yugabyte-provider bin/ysqlsh -h culprit-yugabyte-provider -d culprit_yugabyte -X < dev/yugabyte/seed.sql
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-postgres --lib \
  wire::tests::live_ysql_execution_retains_native_metrics_notices_and_transaction_boundaries -- --ignored
```

Wait until `docker exec culprit-yugabyte-provider bin/ysqlsh -h culprit-yugabyte-provider -X -c 'SELECT 1'` succeeds before creating/seeding the database. The opt-in test connects only to loopback, exercises bounded reads and late errors, captures a server warning, obtains native `EXPLAIN (ANALYZE, DIST)` metrics, and verifies a measured update rolls back. It calls no PostgreSQL storage collector or connection probe.

The dedicated provider is registered as `yugabyte`, sharing only wire transport and SQL execution with PostgreSQL.
See [provider coverage and limits](../../docs/providers/yugabyte.md).

Populate a reproducible statement workload, then verify the native provider:

```sh
docker exec -i culprit-yugabyte-provider bin/ysqlsh -h culprit-yugabyte-provider -d culprit_yugabyte -X < dev/yugabyte/workload.sql
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib live_native_catalog_workload_sessions_and_stale_identity_guards -- --ignored
```

The workload script resets **this disposable container's** statement statistics before running synthetic queries. The
native check verifies catalog/DocDB measurements, recorded plans, scoped sessions and refusal of stale/wrong-node
identities. It does not cancel an unverified session.

Deferred-constraint coverage uses a temporary test schema with CHECK and foreign-key constraints added NOT VALID.
It verifies existing invalid rows are reported, failed validation does not establish validity, and a fresh scan stops
listing the constraints after successful validation. The schema and its synthetic data are removed afterward:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib live_deferred_checks_and_foreign_keys_are_collected_until_validation -- --ignored
```

Verify session progress and cancellation through Workspace, which serves the desktop and CLI, using this test's
own sleeping backend and its full node/PID/start identity:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-engine --test all services::live_native_workspace_session_progress_and_cancel -- --ignored
```

Verify native QPM history with two engine-selected plans for a synthetic statement:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib live_qpm_history_retains_independent_periods_and_redacted_plans -- --ignored
cargo test --locked -p culprit-engine --test all plugin::retained_native_plans_open_offline_without_borrowing_current_catalog_or_measurements
```

The live case verifies independent cache periods, signed plan IDs, redacted cached JSON, current-versus-retained
capture, collector opt-out and one-node global-view counts. It also verifies that unprivileged and `NOINHERIT`
logins are refused before reading the native cache, while an inherited `pg_read_all_stats` grant permits access.
Temporary roles are removed afterward. The offline case uses a provider factory that fails if opened, and checks
that no current catalog, statement runtime or fabricated zero row/execution estimates enter the retained tree.

Verify ASH samples, bounded retention, query identities, restricted-role visibility and one-node global counts:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib live_ash_history_has_its_own_weights_period_bounds_and_role_scope -- --ignored
cargo test --locked -p culprit-checks --lib views::waits::tests
cargo test --locked -p culprit-mcp --lib waits_view::tests
```

The live case sleeps only its own disposable sessions long enough to cross sampling ticks, checks observation nodes,
and verifies that a revoked monitoring grant rejects a stale probe; it performs no query cancellation.
Shared view tests verify independent periods and weights, unknown elapsed time, and the absence of
resource-pressure verdicts from sample shares. Agent-output tests bound the response without duplicating raw history.

Verify native command progress, retained COPY results and current-statement ownership:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib progress -- --include-ignored
```

These cases build a partial index over a bounded synthetic DocDB table and hold a streamed, batched COPY open.
They check native counts without a guessed percentage, successful/failed COPY retention, leading SQL comments,
stale/wrong-node session refusal, hidden-role visibility and a one-node global read. Owned test tables and the
temporary login are removed afterward. Temporary PostgreSQL-local tables cannot verify DocDB batch counters.

## Cluster-view verification

The separate [three-node fixture](cluster.compose.yml) uses the same pinned release, without touching the one-node
demo above. Each container is capped at two CPUs and 2 GiB; it publishes only loopback YSQL ports:
55436/55437/55438 for direct connections and 55439 for Connection Manager on node three.

Start and seed a fresh disposable cluster:

```sh
docker compose -f dev/yugabyte/cluster.compose.yml up -d --wait --wait-timeout 240
docker compose -f dev/yugabyte/cluster.compose.yml exec -T yb1 bin/ysqlsh -h culprit-yb-cluster-1 -X -v ON_ERROR_STOP=1 -c 'CREATE DATABASE culprit_cluster;'
docker compose -f dev/yugabyte/cluster.compose.yml exec -T yb1 bin/ysqlsh -h culprit-yb-cluster-1 -d culprit_cluster -X -v ON_ERROR_STOP=1 < dev/yugabyte/cluster-seed.sql
CULPRIT_YSQL_CLUSTER_TEST=1 cargo test --locked -p culprit-yugabyte --lib cluster_tests:: -- --ignored
```

The database creation command is for a fresh cluster; the seed itself can be repeated. The fixture explicitly enables
global views for its monitoring role and adds the reset/settings views. Production collection does neither.
Its remote-query timeout is set to one second **at startup** to bound failure tests; changing the TServer flag at
runtime did not update existing PostgreSQL backends on the verified release. This test setting is not a production
tuning recommendation.

The cases check 1+2+3 calls aggregated across three nodes, a single-node reset that invalidates subtraction,
unknown index history dates, shared resources, and actual pooled-worker refusal through a direct connection to
the worker's node. Failure cases pause only the uniquely labelled `yb3` fixture container. A drop guard resumes it
even if an assertion fails. They verify warnings and incomplete source evidence, recovery, and that a targeted
session read does not contact an unrelated failed peer. These are local correctness checks, not a scale benchmark.

Remove only this disposable cluster when finished (its synthetic data is discarded):

```sh
docker compose -f dev/yugabyte/cluster.compose.yml down
```

Verify native node resource observations and their distinct administrative permissions:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib live_node_resources_are_shared_native_samples_with_their_own_admin_permissions -- --ignored
cargo test --locked -p culprit-checks --test all provider_audit::shared_node_cpu_and_memory_do_not_become_database_pressure
cargo test --locked -p culprit-mcp --lib waits_view::tests
```

The live case keeps global views off, reads native shared resources, checks the node identity and finite values,
then verifies ordinary-login/`pg_read_all_stats` refusals, `yb_db_admin` access and revocation. It creates no load
spike and changes no cluster limits; temporary roles/grants are removed afterward. Shared view tests ensure that
even busy host CPU and nearly exhausted shared memory do not become a database-pressure verdict. Agent tests
bound node output while retaining totals, unknown values and scope.

Verify unexpected termination records, native login visibility, independent dates, truncation, query identities and
one-node global reads:

```sh
CULPRIT_YSQL_TEST_PORT=55434 cargo test --locked -p culprit-yugabyte --lib live_terminations_preserve_visibility_period_nodes_and_omit_raw_sql -- --ignored
cargo test --locked -p culprit-checks --test all provider_audit::native_terminations_keep_their_own_dates_and_never_become_workload_failure_rates
cargo test --locked -p culprit-mcp --lib waits_view::tests
```

The live case uses 20,000-row sorts with a 64 kB work memory and a session-local zero temporary-file limit on owned
connections to the disposable databases. It induces a bounded resource-limit error; it never sends a process-kill
signal or crashes a backend. Temporary login roles and grants are removed afterward. No raw failed SQL is collected.

[Cluster-wide views](https://docs.yugabyte.com/stable/launch-and-manage/monitor-and-alert/cluster-wide-db-views/) are a disabled-by-default preview in 2026.1.2. Their `server_uuid` identifies each contributing node, and warnings can indicate skipped nodes. A successful SQL response alone does not establish complete cluster coverage.

The default views do not include the statement reset metadata. For testing it in this disposable database, an administrator can add a corresponding global view:

```sql
SELECT yb_create_global_view('public', 'pg_stat_statements_info');
SELECT yb_create_global_view('public', 'pg_settings');
SET yb_enable_global_views = on;
SELECT * FROM public."gv$pg_stat_statements_info";
```

That metadata is needed to distinguish nodes resetting their counters independently. `SET` above enables the feature only in that session. The provider must detect available/configured views, report node scope when cluster views are unavailable, and account for warnings and per-node resets. It must not silently enable a preview feature or perform this administrative DDL during a production scan.

```sh
docker stop culprit-yugabyte-provider
```

Stopping removes this container and its data; no named volume is created.
