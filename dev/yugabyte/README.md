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

The shared transport is groundwork; it does not register YugabyteDB as a PostgreSQL provider. Dedicated collection, engine facts and UI registration are separate work.

## Cluster-view verification

[Cluster-wide views](https://docs.yugabyte.com/stable/launch-and-manage/monitor-and-alert/cluster-wide-db-views/) are a disabled-by-default preview in 2026.1.2. Their `server_uuid` identifies each contributing node, and warnings can indicate skipped nodes. A successful SQL response alone does not establish complete cluster coverage.

The default views do not include the statement reset metadata. For testing it in this disposable database, an administrator can add a corresponding global view:

```sql
SELECT yb_create_global_view('public', 'pg_stat_statements_info');
SET yb_enable_global_views = on;
SELECT * FROM public."gv$pg_stat_statements_info";
```

That metadata is needed to distinguish nodes resetting their counters independently. `SET` above enables the feature only in that session. The provider must detect available/configured views, report node scope when cluster views are unavailable, and account for warnings and per-node resets. It must not silently enable a preview feature or perform this administrative DDL during a production scan.

```sh
docker stop culprit-yugabyte-provider
```

Stopping removes this container and its data; no named volume is created.
