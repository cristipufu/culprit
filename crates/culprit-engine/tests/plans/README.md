# Captured plan comparisons

`samples.json` pairs native plans with a core `DatabaseProduct`; labels and accepted formats come from the engine specifications. `examples/share_sample.rs` parses this catalog once to generate the app's lazy-loaded `fixtures/plan-samples.json` and the browser demo's comparison/export sidecar. The integration tests require exactly one distinct, parseable pair for every declared product and verify that the bundled output is current.

The SQL Server, PostgreSQL, MySQL and MariaDB pairs are the existing demo captures. The TimescaleDB and YugabyteDB pairs were captured on 2026-10-24 from the synthetic databases described in `dev/timescale/README.md` and `dev/yugabyte/README.md`. The new captures retain the servers' actual estimates and measurements, without rewriting them or manufacturing costs.

For each server, run both variants in a read-only transaction with a 10-second statement timeout. The second variant sets `enable_indexscan = off` and `enable_bitmapscan = off` locally before explaining the same SELECT. Roll back after capturing; there is no schema change.

YugabyteDB 2026.1.2.0-b137, using `bin/ysqlsh -X -qAt`:

```sql
/* culprit */ BEGIN READ ONLY;
/* culprit */ SET LOCAL statement_timeout = '10s';
-- For yugabyte-scan.json only:
-- /* culprit */ SET LOCAL enable_indexscan = off;
-- /* culprit */ SET LOCAL enable_bitmapscan = off;
/* culprit */ EXPLAIN (ANALYZE, DIST, FORMAT JSON)
SELECT * FROM public.orders
WHERE created_at = TIMESTAMPTZ '2026-10-14 21:20:00+00';
/* culprit */ ROLLBACK;
```

TimescaleDB 2.31.0 / PostgreSQL 17.11, using `psql -X -qAt`:

```sql
/* culprit */ BEGIN READ ONLY;
/* culprit */ SET LOCAL statement_timeout = '10s';
-- For timescale-scan.json only:
-- /* culprit */ SET LOCAL enable_indexscan = off;
-- /* culprit */ SET LOCAL enable_bitmapscan = off;
/* culprit */ EXPLAIN (ANALYZE, FORMAT JSON)
SELECT observed_at, device_id, temperature
FROM public.sensor_readings
WHERE observed_at >= TIMESTAMPTZ '2026-10-23 00:00:00+00'
ORDER BY observed_at DESC LIMIT 20;
/* culprit */ ROLLBACK;
```

This TimescaleDB window is one uncompressed chunk: its index scan becomes a chunk scan plus sort. The captured relation remains the real chunk name.

SQLite's pair comes from the system `sqlite3` shell against a temporary file created solely for the capture and removed afterward:

```sql
CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, total REAL NOT NULL);
CREATE INDEX orders_customer_idx ON orders(customer_id);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000)
INSERT INTO orders SELECT i, i%100, i/100.0 FROM n;
ANALYZE;
EXPLAIN QUERY PLAN SELECT * FROM orders WHERE customer_id = 42;
EXPLAIN QUERY PLAN SELECT * FROM orders NOT INDEXED WHERE customer_id = 42;
```

SQLite's plans contain neither cost nor runtime measurements; the comparison preserves that absence. Its two plans must never be identified by their shared zero cost.

After updating a capture, run `python3 dev/refresh-demo.py` and review the generated comparisons.
