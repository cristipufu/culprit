\set cid random(1, 200000)
SELECT id, status, total, created_at FROM orders WHERE customer_id = :cid ORDER BY created_at DESC LIMIT 20;
