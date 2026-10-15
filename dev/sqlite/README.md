# SQLite demo

One SQLite file with problems planted on purpose: a foreign key the app filters on with no index
(`orders.customer_id`), the same index twice (`ix_orders_status`, `ix_orders_status_again`), no planner statistics (never
`ANALYZE`d), about 25 MB of free pages left by an emptied staging table, and the default rollback journal.

```sh
# 1. Build it (a few seconds)
rm -f dev/sqlite/demo.db && sqlite3 dev/sqlite/demo.db < dev/sqlite/seed.sql

# 2. Snapshot it, and plan a statement that reads every order to find one customer's
cargo run -p culprit-sqlite --example snapshot -- dev/sqlite/demo.db > fixtures/sqlite-demo.json
cargo run -p culprit-sqlite --example snapshot -- dev/sqlite/demo.db explain \
  "SELECT id, total FROM orders WHERE customer_id = 42 ORDER BY created_at DESC"
```

Culprit opens the file read-only; the demo file isn't committed (`dev/sqlite/demo.db` is in `.gitignore`).
