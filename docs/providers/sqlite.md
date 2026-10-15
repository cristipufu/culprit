# SQLite provider: design

Status: in progress.

## What SQLite can tell Culprit

SQLite is a file, not a server: no sessions, no waits, and no statement statistics (it has no equivalent of
pg_stat_statements or Query Store; `sqlite_stmt` shows only the current connection's prepared statements). What
Culprit can read, opening the file **read-only** (`SQLITE_OPEN_READ_ONLY`, never a write, never a lock beyond a read):

| Snapshot part | SQLite source | Notes |
|---|---|---|
| Server | `sqlite_version()`, `PRAGMA compile_options` | The library Culprit links, not the application's: settings that live in the file are what's reported |
| Tables, columns | `sqlite_schema`, `PRAGMA table_xinfo` | `WITHOUT ROWID` and `STRICT` read from the schema |
| Row counts | `sqlite_stat1` (after `ANALYZE`) | Never `COUNT(*)` on every table: it reads the whole file. Without stats, rows are unknown, not zero |
| Indexes | `PRAGMA index_list`, `PRAGMA index_xinfo` | Partial indexes keep their `WHERE`; no usage counters exist, so "never read" is never claimed |
| Foreign keys | `PRAGMA foreign_key_list` | |
| Size and free space | `PRAGMA page_count`, `page_size`, `freelist_count`; `dbstat` when compiled in | |
| Settings | `PRAGMA journal_mode`, `auto_vacuum`, `page_size`, `user_version` | Per-connection pragmas (`synchronous`, `foreign_keys`, `cache_size`) are the application's choice and can't be read from outside: advice says so instead of guessing |
| Plans | `EXPLAIN QUERY PLAN` | Free to run on any statement: Culprit plans statements itself, read-only |
| Workload | None by itself; optionally a log the framework already writes | See "No app changes" |

## No app changes

Culprit needs nothing from the application: point it at the `.db` file. SQLite is a library inside the application's
process, so nothing outside that process records which statements ran or how long they took; Culprit never asks for a
hook, a driver wrapper or a code change to get that. What it offers is what the file and the planner can tell:

- **From the file:** schema checks, planner statistics, free space and settings (below).
- **From the planner:** the plan of any statement a person pastes or a check names, with `EXPLAIN QUERY PLAN`, read-only.
- **From logs that already exist (optional):** when the application's framework already writes each statement with its
  duration, `culprit scan <connection> --query-log <file>` reads that log: Rails (its log at debug level, the default
  in development) and Django (`django.db.backends` at `DEBUG`), plus JSON lines (`{"sql": …, "ms": …, "at": …}`) for
  anything that already logs that way. Culprit fingerprints each statement (literals out, so repeats of one shape add
  up), ranks the shapes by total time and plans each one. Frameworks whose logs carry no durations by default (Laravel,
  Prisma) aren't asked to add them. The fingerprinter moves from the MySQL provider into core
  (`culprit_core::sql::fingerprint`), so the slow log and this import share one implementation. The log never leaves
  the machine, and literals are already out of every statement Culprit keeps.

## Checks that apply

Opted in after verifying them on the demo file (`dev/sqlite`):

- **Indexes:** unindexed foreign keys (severity from the statements that use the relation when a log is imported;
  otherwise Low, saying why), duplicate and overlapping indexes.
- **Statistics:** no `sqlite_stat1` (the planner guesses), or stats older than large changes when that can be told.
- **Space:** a large freelist (`VACUUM`, or `auto_vacuum = INCREMENTAL` for files that shrink often).
- **Journal:** `DELETE` journal mode on a file read and written at once (WAL lets readers proceed during writes): Low,
  since only the application knows its concurrency.
- **Plans:** full scans of large tables, `USE TEMP B-TREE FOR ORDER BY`, and `AUTOMATIC COVERING INDEX` (SQLite built
  a throwaway index because none fits) from `EXPLAIN QUERY PLAN`.

## Architecture

- `crates/culprit-sqlite`, behind the `sqlite` cargo feature in `registry.rs`, implementing `culprit_core::Provider`
  with `rusqlite` (SQLite compiled in: no runtime dependency). Each source keeps its account in a `Coverage`; a source
  the library lacks (`dbstat`) is a missing capability, never a failed scan.
- `Engine::Sqlite` with its `Dialect`: `"name"` quoting, `CREATE INDEX` (SQLite builds indexes under a write lock:
  advice says so), `DROP INDEX`, `ANALYZE`. Ending a session doesn't exist.
- Connections are a file path (`sqlite:///path/to/app.db`, or a path ending in `.db`, `.sqlite`, `.sqlite3`); no
  secrets, no network.
