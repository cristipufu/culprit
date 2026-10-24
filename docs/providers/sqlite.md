# SQLite

[Engine parity](../parity.md)

Culprit opens SQLite database files read-only. Schema, planner statistics and file allocation are available without changing the application; recorded workload comes from an optional application query log.

## Connecting

Choose **SQLite** and select the database file, or save its path through the CLI.

```sh
culprit conn add app --url db/production.sqlite3
culprit conn set app --query-log log/production.log
```

Paths are stored as file connections, with no network credentials. Culprit can discover a compatible framework log beside the database or in nearby `log`/`logs` directories. An environment-specific database is not paired automatically with another environment's log. A log can also be selected explicitly or disabled on the connection.

## Permissions and setup

Culprit needs read access to the database and any selected query log. It opens the file with read-only flags and `query_only`; it does not add a hook, driver wrapper or required code change to the application.

A framework log must actually include SQL and durations before it can provide performance totals. Unsupported lines or a file without usable statements receive an explanation. Connection-local application settings cannot be discovered by opening a separate connection.

## Workload and plans

SQLite keeps no Query Store or server statement history. Culprit groups logged statements by fingerprint and can report observed runs, total/average/max duration and the spread over the log's own period. Structured error records retain recognized error categories separately; missing durations stay out of performance totals. Statements referring to tables outside this database are excluded where that mismatch can be established.

`EXPLAIN QUERY PLAN` supplies scans, searches, temporary b-trees and automatically built indexes. Shell tree output and pipe-delimited EQP rows can also be imported. The plan has no measured rows, optimizer costs or operator timing. Culprit cannot use its separate file connection to recover the application's historical parameter values or loaded modules.

### If your framework already logs SQL

Culprit reads these logs as they are:

| Framework | What to turn on |
|---|---|
| Rails | The debug log level, the default in development |
| Django | The `django.db.backends` logger at `DEBUG` |
| Entity Framework Core | `Microsoft.EntityFrameworkCore.Database.Command` at `Information` |
| Phoenix / Ecto | The debug log |

Culprit finds the log by itself when it sits beside the database, in a `log/` folder beside it, or in the parent folder's
`log/`. A log named after the database comes first (`production.sqlite3` → `production.log`). Otherwise, point Culprit
at it:

```sh
culprit conn set app --query-log /path/to/production.log
```

In the app, use **Add query log** on the connection.

### Logging queries yourself

With plain `sqlite3` and no ORM (FastAPI, Flask, a script), nothing writes a query log. A small wrapper can, in the
format Culprit reads: one JSON object per line.

```json
{"sql": "SELECT * FROM listings WHERE city = ? AND price < ? LIMIT ?", "ms": 4.2, "at": "2026-10-16T09:12:03.120+00:00"}
```

- `sql`: the statement as you pass it to `execute`, with its `?` placeholders. Culprit removes literal values anyway,
  but logging placeholders keeps your users' data out of the file.
- `ms`: how long it took, in milliseconds.
- `at`: when it ran, as an ISO 8601 timestamp with a time zone. It's optional, but without it Culprit can give shares
  of time and not rates per day.

In Python:

```python
import json, time
from datetime import datetime, timezone

log = open("app.log", "a", buffering=1)

def query(conn, sql, params=()):
    start = time.perf_counter()
    rows = conn.execute(sql, params).fetchall()
    log.write(json.dumps({
        "sql": sql,
        "ms": round((time.perf_counter() - start) * 1000, 3),
        "at": datetime.now(timezone.utc).isoformat(),
    }) + "\n")
    return rows
```

Time the statement until its rows are fetched, not only `execute()`. Python's `sqlite3` returns from `execute()` at the
first row, and SQLite does the rest of the work while the rows are read.

Write the log to a file of its own, named after the database (`app.db` → `app.log`) and kept beside it, and Culprit
finds it on every scan.

### What the report says about a log

- Each shape is ranked by its total time in the log; different `WHERE` clauses are different shapes, listed side by
  side.
- The log covers its own period: the report names it ("the 50 minutes the Rails log covers"), never "the last 50
  minutes".
- The log is read line by line and never kept. Only statement shapes, with values removed, reach the report.
- A database named after an environment is never paired with another environment's log, and statements on tables the
  database doesn't have are left out.

## Live activity and waits

SQLite has no server session list, wait counters, blocking graph, live operator progress or deadlock history for Culprit to query. Those views report the missing capability instead of showing zero activity.

Application logs can record errors such as `SQLITE_BUSY`, but a file inspection does not identify the application's connection or transaction holding a lock. Culprit's SQLite connection is read-only; the editor does not offer committed writes or measured application runs through it.

## Schema and indexes

| Snapshot part |SQLite source | Notes |
|---|---|---|
| Server | `sqlite_version()`, `PRAGMA compile_options` | The library Culprit links, not the application's: settings that live in the file are what's reported |
| Tables, columns | `sqlite_schema`, `PRAGMA table_list`, `PRAGMA table_xinfo` | Ordinary and virtual tables keep their original DDL. SQLite identifies module-owned shadow tables, which are excluded from ordinary table checks; virtual tables stay visible, without guessed rows or sizes. Hidden module columns are omitted; generated columns remain visible |
| Row counts | Counted in the same `dbstat` pass as the sizes; `sqlite_stat1` (after `ANALYZE`) otherwise | Never `COUNT(*)`. A rowid table's rows are its leaf pages' cells, a `WITHOUT ROWID` table's every cell. Unknown, not zero, when neither source can be read |
| Indexes | `PRAGMA index_list`, `PRAGMA index_xinfo` | Partial indexes keep their `WHERE`; no usage counters exist, so "never read" is never claimed |
| Foreign keys | `PRAGMA foreign_key_list` | |
| Size and free space | `PRAGMA page_count`, `page_size`, `freelist_count`; `dbstat` when compiled in | Counted rows and per-object sizes require a whole-file pass within the read budget; see Limits and evidence |
| Settings | `PRAGMA journal_mode`, `auto_vacuum`, `page_size`, `user_version` | Per-connection pragmas (`synchronous`, `foreign_keys`, `cache_size`) are the application's choice and can't be read from outside: advice says so instead of guessing |
| Plans | `EXPLAIN QUERY PLAN` | Free to run on any statement: Culprit plans statements itself, read-only |
| Workload | None by itself; optionally a log the framework already writes | See Workload and plans |

Schema and index advice can identify unindexed foreign keys, duplicate or overlapping indexes, missing per-index planner statistics, large free-page allocation and automatic indexes built by the planner. Without a log, workload impact is unknown and advice stays limited to the file evidence. SQLite records no index-usage counters, so an index is never called unused from an invented zero.

## Limits and evidence

A whole-file `dbstat` read is bounded by both size and time: up to 4 GiB and three seconds in WAL mode, or 128 MiB and half a second with a rollback journal. Beyond those budgets, sizes or counted rows stay unknown and the scan explains why. WAL allocation is read from filesystem metadata; the scan neither checkpoints nor deletes the WAL.

Culprit's linked SQLite library can differ from the application's. Per-connection pragmas such as `foreign_keys`, `synchronous`, `cache_size` and `busy_timeout` describe the application's connection, not a setting Culprit can safely infer from its own.

Logs preserve their recorded period. No timestamps means shares can be shown but per-day rates cannot. Repeated statement shapes do not by themselves prove an N+1 bug; request correlation needs explicit evidence. Raw request identifiers and arbitrary error messages are not retained. See the shared [evidence rules](coverage.md).

### Operational evidence

Additional file and log evidence includes per-index `sqlite_stat1` coverage, WAL allocation from filesystem metadata, recognized structured SQL errors even without durations, and repeated shapes correlated by an explicit `request_id`.

No checkpoint is run and the WAL is never deleted. WAL allocation does not measure uncheckpointed work. Missing durations stay out of performance totals. Raw error messages, literal values and request IDs are not retained. Repetition does not prove an N+1 bug. External logs with shared table names cannot prove database attribution.

## Verification

The [SQLite fixture](../../dev/sqlite/README.md) provides a reproducible database and query logs; the saved SQLite snapshot records library version 3.53.2. Tests cover file-size/read budgets, cancellation, read-only operation, EQP parsing, query-log attachment and missing measurements.

Source declarations are in [`culprit-sqlite/src/sources.rs`](../../crates/culprit-sqlite/src/sources.rs); framework formats and statement grouping live in [`culprit-querylog`](../../crates/culprit-querylog).

Source references: [SQLite error semantics](https://www.sqlite.org/rescode.html).
