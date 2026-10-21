# SQLite provider: design

Status: preview in releases.

## What SQLite can tell Culprit

SQLite is a file, not a server: no sessions, no waits, and no statement statistics (it has no equivalent of
pg_stat_statements or Query Store; `sqlite_stmt` shows only the current connection's prepared statements). What
Culprit can read, opening the file **read-only** (`SQLITE_OPEN_READ_ONLY` and `query_only`: never a write, never a lock
beyond a read, and no long read where the application's writes would wait for it):

| Snapshot part | SQLite source | Notes |
|---|---|---|
| Server | `sqlite_version()`, `PRAGMA compile_options` | The library Culprit links, not the application's: settings that live in the file are what's reported |
| Tables, columns | `sqlite_schema`, `PRAGMA table_list`, `PRAGMA table_xinfo` | Ordinary and virtual tables keep their original DDL. SQLite identifies module-owned shadow tables, which are excluded from ordinary table checks; virtual tables stay visible, without guessed rows or sizes. Hidden module columns are omitted; generated columns remain visible |
| Row counts | Counted in the same `dbstat` pass as the sizes; `sqlite_stat1` (after `ANALYZE`) otherwise | Never `COUNT(*)`. A rowid table's rows are its leaf pages' cells, a `WITHOUT ROWID` table's every cell. Unknown, not zero, when neither source can be read |
| Indexes | `PRAGMA index_list`, `PRAGMA index_xinfo` | Partial indexes keep their `WHERE`; no usage counters exist, so "never read" is never claimed |
| Foreign keys | `PRAGMA foreign_key_list` | |
| Size and free space | `PRAGMA page_count`, `page_size`, `freelist_count`; `dbstat` when compiled in | `dbstat` visits every page in one read transaction. In WAL mode writers carry on beside it, so files up to 4 GB are read (3 s at most). With a rollback journal that read holds the lock writers need to commit, so only files up to 128 MB are read, for half a second at most. Past either, sizes and counted rows are unknown and the scan says why |
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
  duration, Culprit reads that log on every scan. It finds it by itself where frameworks put it: `.log`, `.jsonl` and
  `.ndjson` files beside the database, in a `log` or `logs` folder beside it, or in the parent folder's `log` or `logs`
  (Rails keeps `db/production.sqlite3` and logs to `log/production.log`), one named like the database first, and only a
  file whose first lines hold statements and their durations in a format Culprit reads. A database named for an environment is never paired with another environment's log
  (development timings aren't production's), and statements on tables the database doesn't have are left out, with a
  note (one Rails log serves the app's primary, cache and queue files). A found log is saved on the connection, where
  it can be changed or removed (removing it stops the search); otherwise add it: **Add query log** opens the system's
  file dialog in the app (Culprit opens it, so the window still has no file system access), `culprit conn set <name>
  --query-log <file>` in the CLI, or `--query-log <file>` for one scan. A file without statements is refused with
  why: a framework at its production log level usually leaves SQL out. It reads
  Rails (its log at debug level, the default in development), Django (`django.db.backends` at `DEBUG`), Entity
  Framework Core (`Microsoft.EntityFrameworkCore.Database.Command` at Information), Phoenix/Ecto (its debug log), and
  JSON lines (`{"sql": …, "ms": …, "at": …}`) for anything that already logs that way; the format is told from the
  lines. Culprit fingerprints each statement (literals out, so repeats of one shape add up), ranks the shapes by total
  time and plans the heaviest. Frameworks whose logs carry no durations by default (Laravel, Prisma) aren't asked to
  add them. The log covers its own period: the report says "the 50 minutes the Rails log covers", never "the last 50
  minutes", and a log without timestamps gives shares but no rates. The file is read line by line and never kept; only
  the statement shapes, with literals out, reach the report. One fingerprinter (`culprit_core::fingerprint`) serves
  this import and MySQL's slow query log.

## Checks that apply

Opted in after verifying them on the demo file (`dev/sqlite`):

- **Indexes:** unindexed foreign keys (severity from the statements that use the relation when a log is imported;
  otherwise Low, saying why), duplicate and overlapping indexes.
- **Statistics:** no `sqlite_stat1` (the planner guesses), or stats older than large changes when that can be told.
- **Space:** a large freelist (`VACUUM`, or `auto_vacuum = INCREMENTAL` for files that shrink often).
- **Journal:** the journal mode is shown as a setting, never graded: whether readers and writers contend on the rollback
  journal depends on how the application's processes share the file, which only the application knows.
- **Plans:** `AUTOMATIC COVERING INDEX` (SQLite built a throwaway index because none fits) from `EXPLAIN QUERY PLAN`,
  and the rules that read the statement (a function around a filtered column, a leading wildcard, `NOT IN`). The plan
  carries no row counts or costs, so full scans and `USE TEMP B-TREE` sorts are drawn but never weighed. With a query log, an automatic index is weighed by
  its statement's share of database time (at most Medium: the plan doesn't say what share of the run it is), and when
  its keys are a foreign key's, the key's finding takes it as evidence: one cause, one finding.
- **Index advisor:** the indexes SQLite builds for itself on every run are candidates of their own (`engine-built`),
  ranked by the database time of the statements that make it build them, together with the foreign key's statements
  when it's the same index. SQLite records no missing-index requests; without a log, candidates are foreign keys only.

## Architecture

- `crates/culprit-sqlite`, behind the `sqlite` cargo feature in `registry.rs`, implementing `culprit_core::Provider`
  with `rusqlite` (SQLite compiled in: no runtime dependency). Each source keeps its account in a `Coverage`; a source
  the library lacks (`dbstat`) is a missing capability, never a failed scan.
- `Engine::Sqlite` with its `Dialect`: `"name"` quoting, `CREATE INDEX` (SQLite builds indexes under a write lock:
  advice says so), `DROP INDEX`, `ANALYZE`. Ending a session doesn't exist.
- Connections are a file path (`sqlite:///path/to/app.db`, a path ending in `.db`, `.sqlite`, `.sqlite3` or `.db3`, or
  any path with `--engine sqlite`), kept absolute; no secrets, no network. In the app, picking **SQLite** in the new
  connection form offers **Choose…**, the system's dialog, which Culprit opens itself: the window has no file system access.
- `crates/culprit-querylog` reads framework logs into a `Workload` (one module per format behind a `Format` trait; a
  new framework is one more module). `culprit-engine/src/query_log.rs` attaches it to a scan only when the database
  kept no workload of its own, and plans the heaviest shapes through the provider (placeholders planned as NULL).
- Pasted `EXPLAIN QUERY PLAN` output (the sqlite3 shell's tree or its `id|parent|notused|detail` rows) opens in Plans.
