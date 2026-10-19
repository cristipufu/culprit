# Changelog

Each release's notes are its section below: the release workflow publishes them on GitHub, and the desktop app shows them when it offers the update. A release without a section here isn't drafted.

## 0.3.6

### Fixes

- Findings distinguish needs attention, optional improvements, information and history. Opportunity counts, health colors, digests and exports use the same classification.
- Lock counter comparisons respect the selected time window and distinguish new waits from undated history. Quiet intervals alone no longer verify a fix.
- Query findings lead with measured time and its period. Index candidates need measured workload before offering creation SQL; saved plan statistics remain historical evidence.
- Counter and batching advice preserves application updates, returned values and transaction requirements. Writes through simple CTEs name their underlying table when known.

## 0.3.5

### Fixes

- Saved-file commands apply today's plan and health rules, matching scans opened by connection name.
- PostgreSQL planner cost defaults no longer imply a tuning problem. Saved SQL Server statistics warnings describe compilation-time evidence and ask for current diagnostics before recommending a change.
- Frequent calls no longer prove timer-based polling or a log flush per statement. Statistics maintenance and TOP updates name their real target; empty maintenance showplans no longer count as parser failures, and conditional SQL Server plans retain their own operators and measurements.
- Findings state the cumulative period beside database-wide daily averages, keeping it distinct from query figures measured between Culprit's scans.
- Plan-level warnings on SQL Server's own catalog no longer suggest application schema changes.

## 0.3.4

### Fixes

- Small positive health measurements stay visible: CPU averages such as 0.02275% display as 0.02%, and positive measurements below 0.01% display as <0.01%. Health tiles, resource charts, cloud charts and CLI readings use the same precision; recorded zero and unavailable measurements remain distinct.

## 0.3.3

Health reflects measured database activity and the selected period, with historical and shared counters clearly identified.

### Fixes

- Health shows measured database CPU use instead of grading the scheduling share of historical waits. Cache ratios, lifetime peaks, shared counters and normal version storage no longer imply current pressure on their own.
- Health samples, dated deadlocks and cloud metrics follow the selected period where the source permits it, with limited or cumulative coverage stated explicitly. Sustained resource warnings require enough measured data.
- Compilation counts and timings come from the selected database's Query Store. Retained compilation history is kept separate from runtime windows; unsupported prorated estimates in older scans are withdrawn.
- Query, table, storage, replication and health indicators follow the same assessed findings. Brief queues, old shared averages, bulk reads, inactive bounded replication slots and transient idle transactions no longer receive independent alarming colors.

## 0.3.2

Findings, fixes and plan explanations now use the same evidence across the desktop app, CLI, agent briefs and exports, including when a saved scan is opened again.

### Fixes

- **Recent workload is measured consistently.** When enough scans are available, rates, averages and shares on sources with running totals use what Culprit's own scans measured. Other figures name their counting period. Bursts, occasional jobs, statements that ran once and statements that stopped running are described and ranked for the work they actually did.
- **Plans keep their own measurements.** A captured plan is compared with its own variant's runs, where available. Plans opened in the app retain the measurements from that scan when another scan, connection or workload window is opened, including side-by-side comparisons.
- **Estimates stay estimates.** High page reads alone no longer imply a wrong row estimate or justify statistics, index or forced-plan changes. A residual filter asks for actual row counts and IO before choosing a fix. MySQL's sampled LIMIT values and fixed selectivity guesses no longer justify an index from grouped statement totals.
- **Index advice follows the evidence.** Covering columns are preserved, newer requests on the same keys remain active, unique-key equality lookups do not demand a statistics refresh, and duplicate causes are merged. Statements that stopped running and tables that are gone get no new index of their own.
- **Fixes and verification agree.** Agent briefs identify where each chosen change belongs: application code, a migration, maintenance or database settings. Diagnostic fixes gather evidence first. Verification compares new runs or counter increases after the change, and distinguishes fewer calls, cheaper runs, insert throughput and changes to stored data.
- **Engine-specific guidance is corrected.** PostgreSQL JIT advice respects unknown settings and application overrides. MySQL and MariaDB temporary-table advice distinguishes session limits from shared memory, explains when application connections pick up changes, and does not promise a grouping index removes an aggregate sort. SQL Server planning advice recognizes explicit RECOMPILE hints, and merged lock findings retain each statement's own wait time.
- **SQLite index SQL works in the right schema.** Generated CREATE INDEX statements qualify the index name, preserving quoted names, uniqueness, key order and partial filters. VACUUM guidance includes the temporary disk space it can need.
- **Exports handle unusual text safely.** Database names cannot end comments in generated fix SQL, LIMIT values cannot bypass literal redaction, and non-ASCII text no longer causes a panic in text helpers. Number formatting agrees between Rust and the app.
- **The browser demo stays current.** Saved demo findings, dashboards, index advice, plan insights and share exports are refreshed together with the analysis that produces them.

### For developers

- Golden readings and rules checked across every demo fixture cover findings, briefs, plan explanations and browser output, so an intended fix also checks what users read elsewhere.
- Provider example executables have engine-specific names, fixing output-file collisions in Windows workspace builds. The demo setup commands use the new names.

## 0.3.1

A new connection starts with the engine: pick it, then paste its connection string, or choose the file for SQLite.

### New connection

- **The engine first.** Pick SQL Server, PostgreSQL, MySQL / MariaDB or SQLite. The box that follows is that engine's, with its own example and the formats it reads; SQLite gets a path and **Choose file…** instead of a connection string. The engine you last connected with is picked next time.
- **Your pick settles what the text doesn't.** A bare `host:port/database` on any port, or a .NET connection string such as MySQL's `Server=…;Uid=…`, is read as the engine you picked instead of guessed from the port. A text that names its engine, like a `postgres://` URL or a `.db` path, switches the pick and says so. A connection string pasted before you pick anything picks its engine.
- **What was read, before you connect.** The form shows the user, host, port and database it read, never the password, or that Azure SQL signs in with your az login. User and password fields appear only when the string has none. Enter connects and Escape cancels.
- **SQLite files get a name you'd choose:** `shop/db/production.sqlite3` becomes `shop-production` and `Chinook.db` becomes `chinook`, instead of the whole path.

## 0.3.0

SQLite joins SQL Server, PostgreSQL, MySQL and MariaDB, as a preview. And every check, plan insight and dashboard tile was reviewed again for findings the statistics don't support: 122 reviewed, 87 fixed, each with a test.

### SQLite (preview)

- **Point Culprit at a `.db` file.** In the app, choose **SQLite file…** on a new connection. In the CLI, run `culprit conn add app --url db/production.sqlite3`; a path ending in `.db`, `.sqlite`, `.sqlite3` or `.db3` is enough. Nothing changes in your application: no extension, hook or driver.
- **Safe beside a running app.** The file is opened read-only: no writes, and no lock beyond a read. Its one long read, the page sizes from `dbstat`, is bounded so the application's writers never wait on it: files up to 4 GB in WAL mode, 128 MB with a rollback journal. Row counts come from that read or from `sqlite_stat1`, never from `COUNT(*)`.
- **Your framework's log is the query history.** SQLite records no statements, so Culprit reads the log your framework already writes: Rails, Django, Entity Framework Core, Phoenix/Ecto, or JSON lines. It finds the log beside the database or in its `log` folder, and never pairs a database with another environment's log. It then ranks statement shapes by the time the log measured. You can also pick the log: **Add query log** in the app, `culprit conn set <name> --query-log <file>`, or `--query-log <file>` for one scan. Only statement shapes, with literals removed, reach the report.
- **What it finds:** unindexed foreign keys, duplicate indexes, missing planner statistics (`ANALYZE`), and a large freelist (`VACUUM`). It also finds the indexes SQLite builds for itself on every run because none fits, with the `CREATE INDEX` that builds them once; the index advisor ranks them by the time of the statements that need them. The journal mode is shown but not judged, since only your application knows its concurrency.
- **Plans:** `EXPLAIN QUERY PLAN` is drawn as a plan. Insights point out the indexes SQLite builds on every run and, for statements from your log, filters no index can use (a function around the column, a leading wildcard, `NOT IN`). SQLite's plans carry no row counts or costs, so scans and sorts are shown, not weighed. Paste the sqlite3 shell's output into Plans or `culprit plan show`.
- **What it can't show:** SQLite keeps no usage counters, sessions or waits, so those views say why they're empty instead of showing zeros.

### Fixes

- **Shares of time account for how long a plan was cached.** On SQL Server without Query Store, each cached plan counts from its own compile. A plan cached for a month no longer looks like 90% of database time next to one compiled an hour ago.
- **Server-wide numbers aren't graded as your database's.** The compile ratio, memory waits since startup, connection churn, a busy MySQL server's row-lock waits and other databases' logical replication consumers are reported as the server's, not graded against the database you scanned.
- **Severity needs an amount everywhere.** JIT, missing histograms, WAL volume and read lock waits need about a minute a day, as other time-based findings do. Deadlocks are graded by how often they happen, a failing query needs ten unfinished runs a day for High, and an unstable query needs at least three slow runs.
- **Index advice only where an index can seek.** No advice comes from an `OR` across columns, `<>`, two columns compared, arithmetic, or a cast such as `created_at::date = $1`. None names an optimizer-internal column (`Opt_Bitmap1005`, `Expr1003`). Neither MySQL's fixed 10% guess nor PostgreSQL's default selectivity counts as an estimate. A hash join that matches a tenth or more of the rows it reads gets none either, and pruned partitions and columnstores are skipped.
- **Session statements aren't your app's work.** `BEGIN`, `COMMIT`, `SET` and a pool's `SELECT 1` are no longer "the app is polling" or N+1 loops. A pool's `DISCARD ALL` is named as such, with the fix pointed at the pool.
- **Regressions need a real baseline.** A plan regression needs a 20-run baseline and more CPU or reads per run, not just longer waits. A cold cache right after a restart is Low until a day has passed. Parameter sensitivity is graded on the slow calls' extra time, and SQL Server 2022's per-range plan variants aren't read as one plan reused.
- **How long counters have counted is read carefully.** SQL Server index counters reset by AUTO_CLOSE, a restore or a failover are dated as such. PostgreSQL uses the earlier of its shared resets. MariaDB's user statistics count from when they started, so turning them on doesn't make every index "unused" at once. "Barely read" needs a week of counters, like "never read".
- **Maintenance advice that fits the cause.** Dead rows a vacuum couldn't remove now blame what holds them back, not "vacuum it now". Stale statistics respect the table's own analyze trigger. Aurora, AlloyDB and Neon aren't told to raise `max_wal_size`, which their storage manages.
- **Fixes that hold where they run.** MAXDOP on SQL Server 2014 and older uses the server option. Turning on the slow query log never raises `long_query_time`. `shared_buffers` advice needs a hit ratio under 95% and leaves the size to you. AlloyDB, Neon and Supabase get their own settings controls, and buffer pool advice no longer says "parameter group" on Azure or Google.
- **Plan insights read the plan more carefully.** BitmapAnd, BitmapOr and BRIN bitmap scans aren't misestimates. A fixed ranking weight isn't shown as a share of the plan. Sorts are weighed by measured time, and a function in a seek key counts once per seek. `NOT IN` advice needs the statement to say `NOT IN`. A conversion "stops index use" only when SQL Server's seek warning says so.
- **Dashboard tiles agree with the checks.** The deadlocks tile counts only this database's dated deadlocks. Database time is Watch only at a minute a day. Query Store 90% full under AUTO cleanup isn't a warning. Health drops grades from ratios minutes after a restart, lifetime totals and old peaks. Health squares show the worst first, then the healthy ones. Without query history, the indexes tile doesn't claim no new indexes are needed.
- **Findings rank by severity, then impact.** A Low finding with a big share of a quiet database's time no longer tops Medium ones.

### CLI

- **`culprit sources`** lists what a scan reads on each engine, and which checks each source unlocks.
- **`--query-log <file>`** on `conn add`, `conn set` and `scan`, for SQLite.

### For developers

- **Sources:** each provider declares what it reads in `src/sources.rs` and reads it through one rule: a failing source becomes a note and a locked capability, never a failed scan.
- **Checks:** each check is a type in a file of its own, listed in its area's `CHECKS`. A test fails when a check is written but never listed.
- **One file per engine and per platform:** `EngineSpec`, `Dialect` and `Platform` replace matching on names. A new query-log format is one module in `culprit-querylog`.
- **`docs/ARCHITECTURE.md`** says why the code has this shape, measured against the alternatives.

## 0.2.9

### Safety

- **On PostgreSQL, Culprit's own reads can't be redirected.** A scan's sessions now resolve names in the catalog only (`search_path = pg_catalog, pg_temp`), whatever search_path the database or a role sets, so a schema someone else controls can't stand in for a catalog view or function while Culprit reads statistics. Plans still use the database's own search_path, as the statement's author ran it.
- **Plans fetched on demand follow the same rule as a scan.** Opening a query's plan from a report (in the app, with `culprit explain --query-id`, or through the agent tools) and showing a live session's plan now plan, for a login with more than `pg_monitor`, only the login's own copy of a statement; another role's is refused with the reason and how to get it.
- **"More than pg_monitor" is read more carefully**: both the role a login acts as and the one it logged in as (a `role` setting can make them differ; a login acting as a shared role calls no statement its own), owning the database, and EXECUTE on functions neither PUBLIC nor `pg_monitor` can run (such as `pg_read_file` granted on its own). An extension's functions count as trusted only when a superuser or the login owns the extension. Before PostgreSQL 14, with `pg_stat_statements.track = all`, such a login plans nothing: statements other roles' functions ran on its behalf are recorded as its own.

### Fixes

- **PostgreSQL 12–15 recover from an interrupted plan**: a prepared statement left behind by a failed plan no longer stops every plan after it.

## 0.2.8

Every check, plan insight and collector was reviewed for findings the statistics don't support. Findings are fewer, quieter on quiet databases, and each says only what the scan measured.

### Fixes

- **A report is about the database you scanned.** Deadlocks, the transaction holding up cleanup, plan-cache statements and pending memory grants come from this database only, never from another tenant's on the same server or pool. Signals the server keeps for all its databases (InnoDB's history length, Azure's shared compile and login counters) are said to be the server's and aren't graded as this database's.
- **Severity needs an amount, not just a share.** Every finding about query time (heavy queries, regressions, parameter sensitivity, spills, plan warnings and insights, SQL Server's tuning advice) needs about a minute of database time a day for Medium and ten for High, so a query that takes 40% of a quiet database's time, seconds a day, is Low. A regression that began this morning is rated over the hours since, not diluted by the week before. A chatty statement is Medium only at 60K round trips a day. Quiet databases now show mostly Low findings.
- **Parameter sensitivity is found when several fast plans share the calls.** A statement whose sub-millisecond plans together carry most calls, beside a slow plan still in use, is reported even when no single plan has half of them or an older slow plan has stopped; forcing a plan is offered only when one serves most calls on its own.
- **"Never read" needs a week of counters.** Unused, overlapping and over-indexed findings and the index advisor's clean-ups wait until the usage counters have counted a week, and the advisor says why its list is empty until then.
- **No daily rates from a few minutes of data.** A threshold on a rate needs an hour of history, and "a day" needs 20 hours, unless the count is already past the daily bar. Pressure counted since the server started (THREADPOOL, row-lock waits, connection peaks) is a total with "since" wording, at most Medium.
- **Index advice the database agrees with.** Widening an index keeps it UNIQUE, keeps its filter, key order and ONLINE, and is never offered for a primary key or a clustered index. A disabled or invalid index is brought back rather than built twice. PostgreSQL partitions are judged at their parent, and an index with another operator class (`text_pattern_ops`) isn't a duplicate. An index is suggested only where a scan keeps at most a tenth of the rows it reads.
- **Plans tell the story of the plan in use.** A regression needs the slow plan to have replaced the fast one. Plans used side by side are parameter sensitivity, and SQL Server's own recommendation to force a plan is folded into that query's finding. Estimates are compared per run, and never for an operator that didn't run or that stopped early under a `LIMIT`/`TOP`. A plan insight is more than Low only when the plan does real work.
- **Settings advice holds where it runs.** On RDS, Aurora, Azure and Cloud SQL the fix names the platform's control instead of `ALTER SYSTEM` or `SET GLOBAL`, and no suggestion lowers a setting below today's value.
- **MySQL and MariaDB writes are writes.** A busy single-row `INSERT` is a row-by-row write, not a poll that finds nothing. Rows changed are counted apart from rows sent, and index writes at the table.
- **PostgreSQL counts each statement once**, even when several roles run it, and after a statistics reset it reads row counts from the planner until the next vacuum or analyze. How long its counters have counted comes from PostgreSQL itself (the last reset, or when it last discarded statistics after a crash or restore), never from the server's start time, so an index isn't "never read in a week" days after a crash.
- **MySQL's possible keys are matched by their columns.** In a join, an index counts for a filtered column when it leads with it, by the catalog, not when its name happens to contain the column's.
- **Replicas and standbys aren't reported as lagging or broken when they're only idle or delayed on purpose.** A configured apply delay isn't lag, and a quiet primary's standby is caught up.

### Safety

- **Planning never lends your privileges.** To estimate a statement, PostgreSQL's planner runs the functions it reaches with the privileges of whoever plans it. Culprit now makes every plan in a read-only transaction that is rolled back. A login with more than `pg_monitor` (a superuser, an RDS master user, an Azure admin) plans only the statements it ran itself, and none while functions or operators written by other roles are in reach; the scan says what it left out and how to get those plans: a login with only `pg_monitor` and `SELECT` on the tables plans them all.

### For developers

- **The analysis is a pipeline: prepare, detect, refine, rank.** Checks, prepare steps and refine rules all plug in from outside without forking, and every provider records what its sources could read in one `Coverage`. `docs/ARCHITECTURE.md` maps the code and lists the evidence rules a check must follow.

## 0.2.7

### Fixes

- **A conversion the query writes isn't a type mismatch.** SQL Server warns about every `CONVERT` that may sway a plan, including ones in a select list (`CONVERT(varchar(36), id)`). Culprit read those as an application sending the wrong parameter type and advised switching to varchar. Now only a column SQL Server converts implicitly, to compare it with a value of another type, is a mismatch: in findings, in the plan's insights and in its warnings. One rule, shared by all three.
- **A statement that ran once isn't a pattern.** On every engine, findings that rest only on statements that ran once in the window (an export, a backfill, a query someone ran by hand) are Low, rank low and say so first, with "re-scan later" as the first step. One run is no longer called "stopped running, likely changed by a deploy", and the index advisor doesn't suggest an index for it.
- **No index advice where an index can't help.** When a heavy query's time goes to a scan whose filter keeps more than a tenth of the rows, the advice is to read fewer rows or columns, not to add an index: the same bar the index advisor, filtered scans and plan insights use.
- **"Ran once", not "ran 1 times".**

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
