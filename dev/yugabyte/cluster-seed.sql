-- Disposable three-node fixture only. Never run this setup against an application database.
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
CREATE TABLE IF NOT EXISTS public.cluster_probe (id int PRIMARY KEY, payload text);
INSERT INTO public.cluster_probe SELECT n, repeat('fixture', 20) FROM generate_series(1,200) n
ON CONFLICT DO NOTHING;
CREATE INDEX IF NOT EXISTS cluster_probe_payload_idx ON public.cluster_probe (payload ASC);
ANALYZE public.cluster_probe;

DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='culprit_cluster_monitor') THEN
    CREATE ROLE culprit_cluster_monitor LOGIN;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='culprit_cluster_app') THEN
    CREATE ROLE culprit_cluster_app LOGIN;
  END IF;
END $$;
GRANT pg_read_all_stats, yb_db_admin TO culprit_cluster_monitor;
GRANT SELECT ON public.cluster_probe TO culprit_cluster_app;
ALTER ROLE culprit_cluster_monitor SET yb_enable_global_views=on;
SELECT yb_create_global_view('public','pg_stat_statements_info')
WHERE to_regclass('public."gv$pg_stat_statements_info"') IS NULL;
SELECT yb_create_global_view('public','pg_settings')
WHERE to_regclass('public."gv$pg_settings"') IS NULL;
