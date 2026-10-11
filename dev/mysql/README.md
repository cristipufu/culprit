# MySQL demo

A disposable MySQL 8.4 container with a synthetic `shop` database and a workload that plants problems Culprit should find.

```sh
dev/mysql/setup.sh            # start culprit-mysql-demo on port 33306, seed, run the workload (~1–2 min)
dev/mysql/setup.sh workload   # run the workload again
dev/mysql/setup.sh down       # remove the container

culprit conn add mysql-demo --env local --password-env MYSQL_DEMO_PASSWORD \
  --url 'mysql://root@127.0.0.1:33306/shop?sslmode=disabled'
MYSQL_DEMO_PASSWORD=culprit-demo culprit scan mysql-demo
```

The root password `culprit-demo` is a throwaway for this local container; override it with `MYSQL_DEMO_PASSWORD`.

Planted problems:

- `orders` (500K rows) is filtered by `promo_code`, which has no index, so every lookup reads the whole table.
- "Top spenders" groups 500K orders by customer and channel; in-memory temporary tables are capped at 1 MB, so it goes to disk.
- One product is looked up by id per order line, tens of thousands of times (N+1).
- `ix_customers_region` is never used; `ix_products_sku` duplicates the unique key on `sku`; `ix_orders_status` is a left prefix of `ix_orders_status_created`.
- The InnoDB buffer pool is 32 MB for roughly 150 MB of tables and indexes.
