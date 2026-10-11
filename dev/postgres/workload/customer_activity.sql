\set cid random(1, 200000)
SELECT kind, count(*) FROM events WHERE customer_id = :cid GROUP BY kind;
