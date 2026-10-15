-- A small shop in one SQLite file, with problems planted on purpose (see README.md).
PRAGMA journal_mode = DELETE;   -- the default rollback journal: readers wait while a write commits
PRAGMA foreign_keys = ON;

CREATE TABLE customers (
  id INTEGER PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  region TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE products (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  price REAL NOT NULL
);

-- orders.customer_id references customers, with no index: listing a customer's orders reads every order.
CREATE TABLE orders (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id),
  status TEXT NOT NULL,
  total REAL NOT NULL,
  created_at TEXT NOT NULL
);

-- The same index twice under two names.
CREATE INDEX ix_orders_status ON orders (status);
CREATE INDEX ix_orders_status_again ON orders (status);

CREATE TABLE order_items (
  order_id INTEGER NOT NULL REFERENCES orders(id),
  product_id INTEGER NOT NULL REFERENCES products(id),
  quantity INTEGER NOT NULL,
  PRIMARY KEY (order_id, product_id)
) WITHOUT ROWID;

-- A staging table that was filled and then emptied: its pages stay in the file as free pages.
CREATE TABLE import_staging (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000)
INSERT INTO customers (id, email, region, created_at)
SELECT i, 'customer' || i || '@example.com', CASE i % 4 WHEN 0 THEN 'eu' WHEN 1 THEN 'us' WHEN 2 THEN 'apac' ELSE 'latam' END,
       date('2026-01-01', '+' || (i % 280) || ' days')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500)
INSERT INTO products (id, name, price) SELECT i, 'product ' || i, (i % 97) + 0.99 FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000)
INSERT INTO orders (id, customer_id, status, total, created_at)
SELECT i, (i * 7919) % 5000 + 1, CASE i % 10 WHEN 0 THEN 'returned' WHEN 1 THEN 'pending' ELSE 'shipped' END,
       ((i * 31) % 50000) / 100.0, datetime('2026-01-01', '+' || (i % 280) || ' days', '+' || (i % 86400) || ' seconds')
FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000)
INSERT INTO order_items (order_id, product_id, quantity) SELECT i, (i * 13) % 500 + 1, (i % 3) + 1 FROM n;

WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60000)
INSERT INTO import_staging (id, payload) SELECT i, hex(randomblob(400)) FROM n;
DELETE FROM import_staging;

-- No ANALYZE: the file has no sqlite_stat1, so the planner guesses table sizes and index selectivity.
