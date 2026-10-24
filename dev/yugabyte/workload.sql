\set ON_ERROR_STOP on
-- Disposable fixture only: reset statement counters so setup and admin experiments do not dominate the sample.
SELECT pg_stat_statements_reset() /* culprit */;
SELECT format('SELECT id,status,total FROM public.orders WHERE customer_id = %s LIMIT 20;', i)
FROM generate_series(1,30) i /* culprit */ \gexec
SELECT format('SELECT count(*) FROM public.orders WHERE status = %L;', 'pending')
FROM generate_series(1,30) i /* culprit */ \gexec
SELECT format('SELECT id,total FROM public.orders WHERE created_at >= %L::timestamptz ORDER BY created_at LIMIT 20;', '2026-10-10')
FROM generate_series(1,30) i /* culprit */ \gexec
SELECT format('SELECT name FROM public.customers WHERE id = %s;', i)
FROM generate_series(1,30) i /* culprit */ \gexec
SELECT format('SELECT o.id,c.name FROM public.orders o JOIN public.customers c ON c.id=o.customer_id WHERE o.id < %s;', 21)
FROM generate_series(1,10) i /* culprit */ \gexec
