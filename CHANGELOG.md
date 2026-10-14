# Changelog

Each release's notes are its section below: the release workflow publishes them on GitHub, and the desktop app shows them when it offers the update. A release without a section here isn't drafted.

## 0.2.6

### App

- **Screenshot the view you're looking at.** The camera at the right end of the top bar (or *Screenshot this view* in ⌘K) copies the workspace as a PNG, ready to paste into a chat or a pull request: the top bar and the view, never the sidebar with your connection names. The window takes the picture itself, so it is exactly what's on screen, scroll positions and theme included, at the screen's full resolution. Nothing is saved to disk. macOS for now.

## 0.2.5

### Findings

- **Tables whose rows are mostly large values are found on their own.** A table of 256 MB or more whose rows average 16 KB or more, most of it in pages kept apart from the rows (SQL Server's LOB pages, PostgreSQL's TOAST), is a `large-rows` finding: how big, how many rows, how much of it is large values, and the columns that can hold them, from the catalog without reading any data. It's raised when statements read or write whole rows thousands of times a day (they're listed), when the table grows fast since the previous scan (how much a day), and at 10 GB. The fix says what usually helps: read only the columns a statement needs, keep values many rows repeat in one place, clear values a row no longer needs, check how long deleted rows are kept, and compress what must stay.
- **The schema view shows a table's average row** and how much of it is large values.

### Fixes

- **Row counts on SQL Server without VIEW DATABASE STATE are no longer multiplied for tables with large-value columns.** The catalog-only read counted each table's rows once per kind of page it has (in-row, LOB, row-overflow), so such a table showed up to three times its rows.

## 0.2.4

### Fixes

- **Lock escalations say whether they're new.** SQL Server counts them from when it started and keeps no time for each, so a count from last week read like today's. Each scan now compares with the scan before it, and a scheduled digest with the digest before: escalations since then are High and say how many and since when; a count that hasn't grown is Low and says these are history, none since the previous scan. Without an earlier scan the finding stays High and says when the count began. The fix adds an Extended Events session that keeps the next escalation with its statement, which SQL Server doesn't record otherwise.
- **SQL you run from Culprit no longer shows up as your application's workload.** Culprit now marks every statement you run, measure or change from the CLI or the app's SQL editor as its own, the way it marks the statements it reads statistics with, so Query Store's copy of them stays out of Top queries and Queries; what you wrote and what you get back are unchanged.
- **Missing-index findings name the queries that asked for them**, and when an existing filtered index would serve them if the query used its literal (`is_deleted = 0` rather than a parameter), say so instead of suggesting a new index. When it would serve them only in part, what's left is an index filtered the same way, never one keyed on the filter's column.
- **Rebuilding a filtered index to include more columns keeps its filter, and is suggested only for queries that ask for the rows it holds.** The suggested `DROP_EXISTING` statement left out its `WHERE`, which would have turned it into an index on every row; queries that don't fit the filter, or that Culprit can't show, now get an index of their own.

## 0.2.3

### Fixes

- **On Azure SQL Database, Health no longer turns red over another database's load.** Page life expectancy, batch requests and the plans-compiled ratio come from the performance counters of the server process behind the database, which an elastic pool shares among all its databases. They're now labelled "(whole server)" and shown for context only. The database's own figures from Azure take their place, graded: its storage I/O against its tier's limit over the last hour (with the peak), and its memory in use.
- **Page life expectancy says what it means**: how long a page stays in memory once read. Five minutes or more is healthy; under a minute means pages leave memory soon after they're read, often because of large scans.

## 0.2.2

### Fixes

- **A scan of a filtered index is no longer called a read of the whole table.** A queue polled through an index filtered to its pending rows scans that index, which is usually nearly empty, but the plan reports the whole table's size alongside it, so the filtered-scan and forced-plan checks reported a full read of the table, as High. They now count the rows such a scan reads. Any other scan is still taken at the table's size, so a scan under a TOP that the optimizer expects to stop early is still caught.

## 0.2.1

### Digest

- **`culprit digest` lists the queries that used the most database time** over the window, five by default (`--top-queries N`, 0 leaves them out): what each does, its statement on one line with literal values redacted, the tables it names, its share of database time, runs and average time, and the findings about it.
- **The index advisor's plan is in the digest**: indexes to create (table, keys, includes, why, and the CREATE statement) and to drop or fold into another (unused, duplicate, overlapping, with size and why), Azure SQL's own recommendations included, capped by `--top` with totals for each.
- **Deadlocks are in the digest**, grouped by pattern on the same tables: how many, the latest, how many since the previous digest, the two statements, the lock modes and the fix. When they can't be read, the digest says why instead of reporting none.
- **A scan reads deadlocks over its whole window**, a day at least, so a two-day scan reports two days of them.

### Fixes

- **Microsoft Entra ID sign-in shows one code, once.** Opening a connection asks for a token from several places at once (the report, the dashboard, live polling); each started its own device-code sign-in, so the code on screen kept changing while you typed it, and the sign-ins left waiting held their requests for up to 15 minutes. They now share one sign-in and its token. A sign-in that fails or that you cancel isn't reopened by polling for a minute; `az login` still works straight away.

## 0.2.0

### Updates itself

- **The desktop app keeps itself up to date.** Once a day, in the background and never at start-up's expense, it checks for a newer release and offers it at the bottom of the sidebar. It downloads the update, verifies its signature against the key built into the app, installs it, and restarts into it when you choose. The command palette checks on demand and turns the daily check off.
- **`culprit update` updates the CLI** to the latest release, checking the download against the release's SHA-256 sums. `culprit update --check --json` tells an agent whether a newer release is out. The CLI never checks on its own: an agent that runs `culprit scan` doesn't trigger a network call it didn't ask for.
- **Releases ship signed update packages** for macOS, Windows (the installer), and Linux (the AppImage and the Debian package, each updating itself), plus the bare CLI binaries.

0.1.8 and earlier don't have the updater: install 0.2.0 once by hand, and later releases arrive by themselves.

## 0.1.8

### Findings that match what the database says

- **Lock escalations have their own check, and every escalation is High.** Escalation *attempts*, which SQL Server logs on its own even on idle tables, no longer raise findings.
- **Unused indexes account for their age.** An index that may be less than a week old isn't called unused, overlapping or cleanable, and the schema view says it may be new instead of "never read".
- **Automatic tuning reads as it is.** Plan corrections SQL Server already applied (Verifying, Success) no longer show as suggestions. Forced plans are listed (forced automatically or by hand, since when, and whether the query still runs) with the SQL to unforce them, and a forced plan that scans a large table an index could seek is its own finding.
- **Azure SQL Database's index recommendations show up**, create and drop, with Azure's score and script, merged into the matching missing-index finding.
- **Deadlocks on Azure SQL Database**: `culprit deadlocks` reads the database's deadlock telemetry, which a database user can read.

### Windows in hours

- `--window 6h`, `90m`, `2d` or `1d12h` on `scan`, `dashboard` and `waits`. The app's window picker adds Last hour and Last 6 hours.
- The headline keeps the window you asked for, and says when Query Store's hourly intervals make it cover more.

### App

- **Plan insights copy as Markdown**, all at once from the Insights header or one at a time, with the statement, the fix and how to verify it: ready for a pull request or a coding agent.
