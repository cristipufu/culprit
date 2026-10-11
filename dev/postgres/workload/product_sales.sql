\set pid random(1, 10000)
SELECT count(*), sum(quantity * price) FROM order_items WHERE product_id = :pid;
