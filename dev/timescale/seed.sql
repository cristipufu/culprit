\set ON_ERROR_STOP on
CREATE EXTENSION IF NOT EXISTS timescaledb;
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

CREATE TABLE sensor_readings (
    observed_at timestamptz NOT NULL,
    device_id integer NOT NULL,
    temperature double precision NOT NULL,
    payload text,
    PRIMARY KEY (observed_at, device_id)
);
SELECT create_hypertable('sensor_readings', by_range('observed_at', INTERVAL '1 day'));

INSERT INTO sensor_readings
SELECT timestamptz '2026-10-10 00:00:00+00' + i * INTERVAL '10 seconds',
       i % 100, 15 + (i % 200)::double precision / 10, repeat('sensor-payload-', 8)
FROM generate_series(1, 120000) i;

ALTER TABLE sensor_readings SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'device_id',
    timescaledb.compress_orderby = 'observed_at DESC'
);
SELECT compress_chunk(chunk) FROM show_chunks('sensor_readings', older_than => timestamptz '2026-10-17 00:00:00+00') chunk;
ANALYZE sensor_readings;

CREATE TABLE devices (id integer PRIMARY KEY, name text NOT NULL);
INSERT INTO devices SELECT i, 'sensor-' || i FROM generate_series(0, 99) i;
ANALYZE devices;

CREATE MATERIALIZED VIEW sensor_hourly WITH (timescaledb.continuous) AS
SELECT time_bucket(INTERVAL '1 hour', observed_at) AS bucket, device_id, avg(temperature) AS temperature
FROM sensor_readings GROUP BY 1, 2 WITH NO DATA;
