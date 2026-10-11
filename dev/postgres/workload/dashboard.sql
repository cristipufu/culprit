SELECT status, count(*), sum(total) FROM orders WHERE created_at > now() - interval '7 days' GROUP BY status;
