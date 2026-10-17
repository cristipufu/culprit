# Postgres demo

A disposable Postgres with problems planted on purpose: an unindexed foreign key the app filters on, an unused index, a duplicate and a redundant index, a table with dead rows and stale statistics (autovacuum off), a sort that spills past `work_mem`, and an N+1 lookup.

```sh
# 1. Start Postgres with pg_stat_statements loaded (port 55432)
docker run -d --name culprit-pg-demo -p 55432:5432 -e POSTGRES_PASSWORD=culprit -e POSTGRES_DB=demo \
  -v "$PWD/dev/postgres:/demo:ro" postgres:18.6 \
  -c shared_preload_libraries=pg_stat_statements -c track_io_timing=on

# 2. Load the schema and data (~1 minute), then run ~30 s of workload
docker exec culprit-pg-demo psql -U postgres -d demo -f /demo/seed.sql
docker exec culprit-pg-demo /demo/workload.sh

# 3. Snapshot it
CULPRIT_PG_URL='postgres://postgres:culprit@localhost:55432/demo?sslmode=disable' \
  cargo run -p culprit-postgres --example postgres_snapshot > fixtures/postgres-demo.json
```

`culprit_app` (password `culprit`) is a login without `pg_monitor`, to see how Culprit degrades. Remove everything with `docker rm -f culprit-pg-demo`.
