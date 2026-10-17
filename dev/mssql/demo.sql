-- A synthetic online shop with planted performance problems, for developing and demoing Culprit.
-- Every problem below is something a real application ships by accident:
--   * customers.email is varchar, the application sends nvarchar parameters      -> type mismatch, scan instead of seek
--   * orders.shipped_at and order_items.order_id have no index but are filtered   -> missing index / full scans
--   * ticket_events.ticket_id is a foreign key with no index                      -> unindexed foreign key
--   * ix_products_sku duplicates the unique constraint on sku                     -> duplicate index
--   * ix_orders_status is a left prefix of ix_orders_status_created              -> redundant index
--   * ix_orders_channel is never read                                            -> unused index
--   * support_tickets.priority is heavily skewed and served by a key lookup plan -> parameter-sensitive query
-- The N+1 product lookup lives in the workload (crates/culprit-mssql/examples/mssql_workload.rs).
SET NOCOUNT ON;
GO
IF DB_ID('culprit_demo') IS NOT NULL
BEGIN
    ALTER DATABASE culprit_demo SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE culprit_demo;
END
GO
CREATE DATABASE culprit_demo;
GO
ALTER DATABASE culprit_demo SET RECOVERY SIMPLE;
ALTER DATABASE culprit_demo SET QUERY_STORE = ON
    (OPERATION_MODE = READ_WRITE, QUERY_CAPTURE_MODE = ALL, INTERVAL_LENGTH_MINUTES = 5, DATA_FLUSH_INTERVAL_SECONDS = 60);
GO
USE culprit_demo;
GO
CREATE SCHEMA shop;
GO
CREATE TABLE shop.customers (
    id int IDENTITY CONSTRAINT pk_customers PRIMARY KEY,
    email varchar(200) NOT NULL,
    name nvarchar(100) NOT NULL,
    country_code char(2) NOT NULL,
    segment varchar(20) NOT NULL,
    created_at datetime2(0) NOT NULL
);
CREATE INDEX ix_customers_email ON shop.customers (email);

CREATE TABLE shop.products (
    id int IDENTITY CONSTRAINT pk_products PRIMARY KEY,
    sku varchar(32) NOT NULL CONSTRAINT uq_products_sku UNIQUE,
    name nvarchar(200) NOT NULL,
    category varchar(40) NOT NULL,
    price decimal(10, 2) NOT NULL,
    is_active bit NOT NULL
);
CREATE INDEX ix_products_sku ON shop.products (sku);

CREATE TABLE shop.orders (
    id bigint IDENTITY CONSTRAINT pk_orders PRIMARY KEY,
    customer_id int NOT NULL CONSTRAINT fk_orders_customer REFERENCES shop.customers (id),
    status varchar(20) NOT NULL,
    channel varchar(20) NOT NULL,
    total decimal(12, 2) NOT NULL,
    created_at datetime2(0) NOT NULL,
    shipped_at datetime2(0) NULL
);
CREATE INDEX ix_orders_customer_id ON shop.orders (customer_id, created_at);
CREATE INDEX ix_orders_status ON shop.orders (status);
CREATE INDEX ix_orders_status_created ON shop.orders (status, created_at);
CREATE INDEX ix_orders_channel ON shop.orders (channel);

CREATE TABLE shop.order_items (
    id bigint IDENTITY CONSTRAINT pk_order_items PRIMARY KEY,
    order_id bigint NOT NULL CONSTRAINT fk_order_items_order REFERENCES shop.orders (id),
    product_id int NOT NULL CONSTRAINT fk_order_items_product REFERENCES shop.products (id),
    quantity int NOT NULL,
    unit_price decimal(10, 2) NOT NULL
);
CREATE INDEX ix_order_items_product_id ON shop.order_items (product_id);

CREATE TABLE shop.support_tickets (
    id int IDENTITY CONSTRAINT pk_support_tickets PRIMARY KEY,
    customer_id int NOT NULL CONSTRAINT fk_support_tickets_customer REFERENCES shop.customers (id),
    priority tinyint NOT NULL,
    status varchar(20) NOT NULL,
    subject nvarchar(200) NOT NULL,
    opened_at datetime2(0) NOT NULL,
    closed_at datetime2(0) NULL
);
CREATE INDEX ix_support_tickets_customer_id ON shop.support_tickets (customer_id);
CREATE INDEX ix_support_tickets_priority ON shop.support_tickets (priority);

CREATE TABLE shop.ticket_events (
    id bigint IDENTITY CONSTRAINT pk_ticket_events PRIMARY KEY,
    ticket_id int NOT NULL CONSTRAINT fk_ticket_events_ticket REFERENCES shop.support_tickets (id),
    event_type varchar(30) NOT NULL,
    created_at datetime2(0) NOT NULL,
    note nvarchar(200) NULL
);
GO

CREATE TABLE #n (i int NOT NULL PRIMARY KEY);
INSERT INTO #n (i)
SELECT TOP (1500000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL))
FROM sys.all_columns a CROSS JOIN sys.all_columns b CROSS JOIN (VALUES (1), (2), (3)) c (x);

INSERT INTO shop.customers WITH (TABLOCK) (email, name, country_code, segment, created_at)
SELECT CONCAT('user', i, '@example.com'),
       CONCAT(N'Customer ', i),
       CHOOSE(i % 5 + 1, 'US', 'DE', 'FR', 'GB', 'RO'),
       CHOOSE(i % 3 + 1, 'retail', 'business', 'vip'),
       DATEADD(minute, -i * 3, '2026-10-01')
FROM #n WHERE i <= 200000;

INSERT INTO shop.products WITH (TABLOCK) (sku, name, category, price, is_active)
SELECT CONCAT('SKU-', RIGHT(CONCAT('000000', i), 6)),
       CONCAT(N'Product ', i),
       CHOOSE(i % 8 + 1, 'books', 'games', 'garden', 'kitchen', 'music', 'office', 'sports', 'toys'),
       i % 500 + 9.99,
       CASE WHEN i % 10 = 0 THEN 0 ELSE 1 END
FROM #n WHERE i <= 20000;

INSERT INTO shop.orders WITH (TABLOCK) (customer_id, status, channel, total, created_at, shipped_at)
SELECT 1 + ABS(CHECKSUM(CAST(i AS bigint) * 7919)) % 200000,
       CASE WHEN i % 100 < 2 THEN 'pending' WHEN i % 100 < 5 THEN 'cancelled' ELSE 'delivered' END,
       CHOOSE(i % 3 + 1, 'web', 'app', 'store'),
       (i % 900) + 4.5,
       DATEADD(second, -i * 25, '2026-10-01'),
       CASE WHEN i % 100 >= 5 THEN DATEADD(hour, 30, DATEADD(second, -i * 25, '2026-10-01')) END
FROM #n WHERE i <= 1000000;

INSERT INTO shop.order_items WITH (TABLOCK) (order_id, product_id, quantity, unit_price)
SELECT 1 + (i - 1) % 1000000,
       1 + ABS(CHECKSUM(CAST(i AS bigint) * 104729)) % 20000,
       1 + i % 3,
       (i % 500) + 9.99
FROM #n WHERE i <= 1500000;

INSERT INTO shop.support_tickets WITH (TABLOCK) (customer_id, priority, status, subject, opened_at, closed_at)
SELECT 1 + ABS(CHECKSUM(CAST(i AS bigint) * 31)) % 200000,
       CASE WHEN i % 1000 = 0 THEN 5 WHEN i % 100 = 0 THEN 4 WHEN i % 10 = 0 THEN 3 ELSE 1 END,
       CHOOSE(i % 4 + 1, 'open', 'waiting', 'solved', 'closed'),
       CONCAT(N'Ticket about order ', i),
       DATEADD(minute, -i * 2, '2026-10-01'),
       CASE WHEN i % 4 >= 2 THEN DATEADD(hour, 6, DATEADD(minute, -i * 2, '2026-10-01')) END
FROM #n WHERE i <= 300000;

INSERT INTO shop.ticket_events WITH (TABLOCK) (ticket_id, event_type, created_at, note)
SELECT 1 + (i - 1) % 300000,
       CHOOSE(i % 4 + 1, 'opened', 'replied', 'escalated', 'closed'),
       DATEADD(minute, -i, '2026-10-01'),
       CASE WHEN i % 5 = 0 THEN N'Customer replied by email' END
FROM #n WHERE i <= 800000;

DROP TABLE #n;
GO
-- Start the workload with a clean history: the seed itself is not the application's workload.
ALTER DATABASE culprit_demo SET QUERY_STORE CLEAR;
GO
