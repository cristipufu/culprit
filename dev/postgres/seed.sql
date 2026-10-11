-- A small shop database with problems planted on purpose, so every Postgres check has something to find.
\set ON_ERROR_STOP on
SET client_min_messages = warning;

CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

DROP TABLE IF EXISTS events, order_items, orders, products, customers CASCADE;

CREATE TABLE customers (
    id         integer PRIMARY KEY,
    email      text NOT NULL,
    name       text NOT NULL,
    region     text NOT NULL,
    created_at timestamptz NOT NULL
);

CREATE TABLE products (
    id    integer PRIMARY KEY,
    sku   text NOT NULL UNIQUE,
    name  text NOT NULL,
    price numeric(10, 2) NOT NULL
);

-- Problem: orders.customer_id is a foreign key with no index, and the app looks orders up by customer.
CREATE TABLE orders (
    id          bigint PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES customers (id),
    status      text NOT NULL,
    total       numeric(12, 2) NOT NULL,
    note        text,
    created_at  timestamptz NOT NULL
);

-- Problem: order_items.product_id is a foreign key with no index.
CREATE TABLE order_items (
    id         bigint PRIMARY KEY,
    order_id   bigint NOT NULL REFERENCES orders (id),
    product_id integer NOT NULL REFERENCES products (id),
    quantity   integer NOT NULL,
    price      numeric(10, 2) NOT NULL
);

-- Problem: autovacuum is off for events, so dead rows pile up and statistics go stale.
CREATE TABLE events (
    id          bigint PRIMARY KEY,
    customer_id integer NOT NULL,
    kind        text NOT NULL,
    payload     jsonb,
    created_at  timestamptz NOT NULL
) WITH (autovacuum_enabled = false);

INSERT INTO customers
SELECT g, 'user' || g || '@example.com', 'Customer ' || g,
       (ARRAY['eu', 'us', 'apac', 'latam'])[1 + g % 4],
       now() - (g % 1000) * interval '1 day'
FROM generate_series(1, 200000) AS g;

INSERT INTO products
SELECT g, 'SKU-' || lpad(g::text, 6, '0'), 'Product ' || g, (1 + g % 500)::numeric + 0.99
FROM generate_series(1, 10000) AS g;

INSERT INTO orders
SELECT g, 1 + (hashint4(g)::bigint & 2147483647) % 200000,
       (ARRAY['new', 'paid', 'shipped', 'delivered', 'cancelled'])[1 + g % 5],
       ((g % 50000) / 100.0)::numeric(12, 2),
       md5(g::text) || ' ' || md5((g * 7)::text),
       now() - ((g % 365) * interval '1 day') - ((g % 86400) * interval '1 second')
FROM generate_series(1, 2000000) AS g;

INSERT INTO order_items
SELECT g, 1 + (g - 1) / 2, 1 + g % 10000, 1 + g % 4, ((g % 500) + 0.99)::numeric
FROM generate_series(1, 1000000) AS g;

INSERT INTO events
SELECT g, 1 + g % 200000, (ARRAY['view', 'click', 'cart', 'checkout'])[1 + g % 4],
       jsonb_build_object('page', '/p/' || (g % 1000)), now() - (g % 30) * interval '1 day'
FROM generate_series(1, 300000) AS g;

CREATE INDEX events_customer_idx ON events (customer_id);

-- Problem: an exact duplicate of the unique constraint's index.
CREATE INDEX products_sku_idx ON products (sku);
-- Problem: orders_status_idx is a left prefix of orders_status_created_idx, and neither is used.
CREATE INDEX orders_status_idx ON orders (status);
CREATE INDEX orders_status_created_idx ON orders (status, created_at);
-- Problem: an index nothing reads.
CREATE INDEX customers_region_idx ON customers (region);

ALTER TABLE orders ADD CONSTRAINT orders_total_check CHECK (total >= 0);

ANALYZE;

-- After the last ANALYZE: churn on events so it has dead rows and stale statistics.
UPDATE events SET payload = payload || '{"seen": true}' WHERE id % 2 = 0;
DELETE FROM events WHERE id % 10 = 0;
INSERT INTO events
SELECT g, 1 + g % 200000, 'view', '{}'::jsonb, now()
FROM generate_series(300001, 500000) AS g;

-- A login without monitoring rights, to see how Culprit degrades.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'culprit_app') THEN
        CREATE ROLE culprit_app LOGIN PASSWORD 'culprit';
    END IF;
END $$;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO culprit_app;

SELECT pg_stat_statements_reset();
SELECT pg_stat_reset();
