SELECT id, note FROM orders WHERE created_at > now() - interval '45 days' ORDER BY note;
