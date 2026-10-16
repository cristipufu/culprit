# SQLite demo

One SQLite file with problems planted on purpose: a foreign key the app filters on with no index
(`orders.customer_id`), the same index twice (`ix_orders_status`, `ix_orders_status_again`), no planner statistics (never
`ANALYZE`d), about 25 MB of free pages left by an emptied staging table, and the default rollback journal.

```sh
# 1. Build it (a few seconds)
rm -f dev/sqlite/demo.db && sqlite3 dev/sqlite/demo.db < dev/sqlite/seed.sql

# 2. The Rails log an app on it would write: 90 minutes of requests, the same every time
python3 dev/sqlite/app-log.py > dev/sqlite/development.log

# 3. Scan it with the log, as the app would after "Add query log"
culprit conn add sqlite-demo --url dev/sqlite/demo.db --query-log dev/sqlite/development.log
culprit scan sqlite-demo

# Or poke at the file directly: a snapshot, or the plan of one statement
cargo run -p culprit-sqlite --example snapshot -- dev/sqlite/demo.db
cargo run -p culprit-sqlite --example snapshot -- dev/sqlite/demo.db explain \
  "SELECT id, total FROM orders WHERE customer_id = 42 ORDER BY created_at DESC"
```

`fixtures/sqlite-demo.json` is the snapshot of step 3 (`culprit scan sqlite-demo --json --full`, its `snapshot`), so the
browser demo shows the queries the log holds. Culprit opens the file read-only; neither the file nor the log is committed
(both are in `.gitignore`).
