-- Synthetic `shop` schema with planted performance problems (see README.md for the list).
DROP DATABASE IF EXISTS shop;
CREATE DATABASE shop;
USE shop;

CREATE TABLE digits (d TINYINT PRIMARY KEY);
INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
CREATE TABLE seq (n INT PRIMARY KEY);
INSERT INTO seq
SELECT a.d + 10 * b.d + 100 * c.d + 1000 * e.d + 10000 * f.d + 100000 * g.d
  FROM digits a, digits b, digits c, digits e, digits f, digits g;

CREATE TABLE customers (
  id INT AUTO_INCREMENT PRIMARY KEY,
  email VARCHAR(120) NOT NULL,
  name VARCHAR(80) NOT NULL,
  region VARCHAR(16) NOT NULL,
  created_at DATETIME NOT NULL,
  KEY ix_customers_region (region)                      -- planted: never used by the workload
);
INSERT INTO customers (email, name, region, created_at)
SELECT CONCAT('customer', n, '@example.com'), CONCAT('Customer ', n),
       ELT(1 + n % 5, 'eu-west', 'eu-north', 'us-east', 'us-west', 'ap-south'),
       '2025-01-01' + INTERVAL (n % 600) DAY
  FROM seq WHERE n < 200000;

CREATE TABLE products (
  id INT AUTO_INCREMENT PRIMARY KEY,
  sku VARCHAR(32) NOT NULL,
  name VARCHAR(80) NOT NULL,
  category VARCHAR(32) NOT NULL,
  price DECIMAL(10, 2) NOT NULL,
  UNIQUE KEY uq_products_sku (sku),
  KEY ix_products_sku (sku)                             -- planted: duplicates uq_products_sku
);
INSERT INTO products (sku, name, category, price)
SELECT CONCAT('SKU-', LPAD(n, 6, '0')), CONCAT('Product ', n), ELT(1 + n % 8, 'books', 'games', 'garden', 'tools', 'music', 'toys', 'sport', 'food'),
       5 + (n % 200)
  FROM seq WHERE n < 20000;

CREATE TABLE orders (
  id INT AUTO_INCREMENT PRIMARY KEY,
  customer_id INT NOT NULL,
  status VARCHAR(16) NOT NULL,
  channel VARCHAR(16) NOT NULL,
  promo_code VARCHAR(16) NULL,                          -- planted: filtered on, never indexed
  total DECIMAL(12, 2) NOT NULL,
  created_at DATETIME NOT NULL,
  shipped_at DATETIME NULL,
  KEY ix_orders_status (status),                        -- planted: left prefix of the next index
  KEY ix_orders_status_created (status, created_at),
  CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers (id)
);
INSERT INTO orders (customer_id, status, channel, promo_code, total, created_at, shipped_at)
SELECT 1 + (n * 7919) % 200000,
       ELT(1 + n % 4, 'pending', 'paid', 'shipped', 'cancelled'),
       ELT(1 + n % 3, 'web', 'app', 'store'),
       IF(n % 50 = 0, CONCAT('SPRING', n % 10), NULL),
       10 + (n % 900) + (n % 7) / 10,
       '2026-01-01' + INTERVAL (n % 280) DAY + INTERVAL (n % 86400) SECOND,
       IF(n % 4 = 2, '2026-01-02' + INTERVAL (n % 280) DAY, NULL)
  FROM seq WHERE n < 500000;

CREATE TABLE order_items (
  id INT AUTO_INCREMENT PRIMARY KEY,
  order_id INT NOT NULL,
  product_id INT NOT NULL,
  quantity INT NOT NULL,
  price DECIMAL(10, 2) NOT NULL,
  CONSTRAINT fk_items_order FOREIGN KEY (order_id) REFERENCES orders (id),
  CONSTRAINT fk_items_product FOREIGN KEY (product_id) REFERENCES products (id)
);
INSERT INTO order_items (order_id, product_id, quantity, price)
SELECT 1 + n % 500000, 1 + (n * 31) % 20000, 1 + n % 3, 5 + n % 200
  FROM seq WHERE n < 800000;

DROP TABLE seq;
DROP TABLE digits;
ANALYZE TABLE customers, products, orders, order_items;
