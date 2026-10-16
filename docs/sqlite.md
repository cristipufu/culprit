# Culprit on SQLite (preview)

Point Culprit at your `.db` file. It opens the file read-only and needs nothing from your app: no extension, hook or
driver.

```sh
culprit conn add app --url db/app.db
culprit scan app
```

In the desktop app, pick **SQLite** on a new connection and choose the file.

## What it finds from the file alone

- Unindexed foreign keys and duplicate indexes
- Missing planner statistics (`ANALYZE`) and a large freelist (`VACUUM`)
- The plan of any statement you give it (`EXPLAIN QUERY PLAN`), including the indexes SQLite builds for itself on every
  run because none fits

## Ranking queries by total time

SQLite doesn't record which statements ran or how long they took. To rank queries by their share of database time,
Culprit reads a log your app writes. It groups repeats of the same statement into one shape, so a query run thousands
of times with different values adds up, and it plans the heaviest shapes.

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

## What the report says about a log

- Each shape is ranked by its total time in the log; different `WHERE` clauses are different shapes, listed side by
  side.
- The log covers its own period: the report names it ("the 50 minutes the Rails log covers"), never "the last 50
  minutes".
- The log is read line by line and never kept. Only statement shapes, with values removed, reach the report.
- A database named after an environment is never paired with another environment's log, and statements on tables the
  database doesn't have are left out.

How the SQLite provider works inside, and why: [providers/sqlite.md](providers/sqlite.md).
