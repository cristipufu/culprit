\set ON_ERROR_STOP on
SELECT pg_stat_statements_reset() /* culprit */;
SELECT format('SELECT observed_at, temperature FROM public.sensor_readings WHERE device_id = %s AND observed_at >= timestamptz ''2026-10-17'' ORDER BY observed_at DESC LIMIT 50;', i % 100)
FROM generate_series(1, 100) i /* culprit */ \gexec
SELECT 'SELECT device_id, avg(temperature) FROM public.sensor_readings WHERE observed_at >= timestamptz ''2026-10-10'' GROUP BY device_id;'
FROM generate_series(1, 100) i /* culprit */ \gexec
SELECT format('SELECT name FROM public.devices WHERE id = %s;', i % 100)
FROM generate_series(1, 100) i /* culprit */ \gexec
