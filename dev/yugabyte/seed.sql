\set ON_ERROR_STOP on
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
CREATE TABLE customers (id integer PRIMARY KEY, region text NOT NULL, name text NOT NULL);
INSERT INTO customers SELECT i, 'region-' || (i % 8), 'customer-' || i FROM generate_series(1, 2000) i;

CREATE TABLE orders (
    id bigint PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES customers(id),
    status text NOT NULL,
    total numeric(12,2) NOT NULL,
    created_at timestamptz NOT NULL,
    notes text
);
INSERT INTO orders
SELECT i, 1 + i % 2000, CASE WHEN i % 20 = 0 THEN 'pending' ELSE 'complete' END,
       (i % 10000)::numeric / 100, timestamptz '2026-10-01 00:00:00+00' + i * INTERVAL '1 minute',
       repeat('order-note-', 8)
FROM generate_series(1, 20000) i;
CREATE INDEX orders_created_idx ON orders (created_at);
ANALYZE customers;
ANALYZE orders;
