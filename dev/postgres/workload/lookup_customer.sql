\set cid random(1, 200000)
SELECT name, email FROM customers WHERE id = :cid;
