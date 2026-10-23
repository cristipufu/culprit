# TimescaleDB development database

Verified with TimescaleDB **2.31.0**, PostgreSQL **17.11**, on an arm64 container. The seed creates 120,000 sensor readings in 14 chunks, compresses seven chunks, adds an ordinary table, and creates an unpopulated continuous aggregate. All data is synthetic.

```sh
docker run --rm -d --name culprit-timescale-provider --cpus 2 --memory 1g \
  -p 127.0.0.1:55435:5432 \
  -e POSTGRES_PASSWORD=culprit -e POSTGRES_DB=culprit_timescale \
  timescale/timescaledb@sha256:d971568f9af05e218866181151117f517282f05f299a0e4010dc6a97704f6f2c \
  -c shared_preload_libraries=timescaledb,pg_stat_statements
docker exec -i culprit-timescale-provider psql -U postgres -d culprit_timescale -X < dev/timescale/seed.sql
docker exec -i culprit-timescale-provider psql -U postgres -d culprit_timescale -X < dev/timescale/workload.sql
CULPRIT_PG_URL='postgres://postgres:culprit@127.0.0.1:55435/culprit_timescale?sslmode=disable' \
  CULPRIT_CONNECTION=timescale-demo \
  cargo run --locked -p culprit-postgres --example postgres_snapshot -- snapshot
```

Wait for `docker exec culprit-timescale-provider pg_isready -U postgres` to succeed before seeding. The workload resets this demo's statement counters; run it only on the disposable database.

Connect the development desktop app using **PostgreSQL** and the same URL. Open Schema → `public.sensor_readings` → Managed chunks. The saved `fixtures/timescale-demo.report.json` is from this database; the browser demo reads it without a database connection. See [provider behavior and limitations](../../docs/providers/timescale.md).

```sh
docker stop culprit-timescale-provider
```

Stopping removes the disposable container and its database. No named volume is created.
