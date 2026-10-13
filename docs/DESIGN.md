# Culprit — design

Culprit connects to a database and tells you, in plain language, what is making it slow and what to do about it. It ships as a desktop app (Tauri + React) and a CLI (`culprit`) built on the same Rust core, so a coding agent sees exactly what a person sees.

## Principles

1. **Connect → answers.** The first screen after connecting is the dashboard: what needs a look, most urgent first, each tile a plain sentence with the ranked findings one keystroke away (`2`). Never a schema tree. A scan reads only statistics views and finishes in seconds.
2. **Human first, numbers second.** Every finding opens with one sentence a non-DBA understands, then the evidence.
3. **Rank by impact.** Order by share of database time (or the resource the finding wastes), never by how many warnings fired.
4. **Every finding is actionable.** Evidence → Fix → Verify. A finding without a next step is not shipped.
5. **Safe on production by construction.** Read-only sessions, short timeouts, nothing that executes user queries without an explicit, per-action confirmation.
6. **Agent-first.** Everything the app shows, the CLI prints as stable JSON with stable IDs. Agents reference connections by name and never see credentials.
7. **Fast.** Window on screen in < 300 ms, last scan rendered instantly from disk, fresh scan in < 5 s, every interaction < 100 ms, keyboard for everything.
8. **Honest.** Each finding states its confidence and the data window it is based on ("Query Store, last 7 days"). Missing permissions or features are shown as locked checks with the exact fix, never as silence.

## Who it is for

| Who | Job | Where they start |
|---|---|---|
| App developer | "What's the low-hanging fruit in alpha before we ship?" | Dashboard → Findings |
| On-call engineer | "The API is slow right now. What is the database doing?" | Live |
| Coding agent | "Did my migration or query make things worse? What should I index?" | `culprit scan --json`, `culprit explain` |

## Information architecture

```
Connections (start)
└─ Connected workspace
   ├─ Dashboard  the database at a glance; where it opens     (1)
   ├─ Findings   ranked opportunities + finding detail      (2)
   ├─ Queries    workload explorer + query detail + plan     (3)
   ├─ Live       sessions, blocking chains, waits           (4)
   ├─ Schema     table and index health (not a browser)      (5)
   ├─ Editor     SQL, explain, run, compare plans            (6)
   └─ Plans      open, paste and compare plans               (7)
⌘K command palette reaches every view, finding, query, table and action.
```

## Screens

### Connections

- Recent connections as cards: environment colour (production red, staging amber, development blue, local green), engine, last scan summary ("14 opportunities · 2 h ago").
- **One smart input**: paste anything — `postgres://`, `mssql://`, ADO (`Server=…;Database=…`), JDBC, or a bare host. The engine and fields are detected; extra fields appear only when needed.
- Auth: password, Microsoft Entra ID (Azure CLI token, no secret stored), Postgres `.pgpass`.
- Environment tag is required and drives safety rules.
- **Find Azure SQL databases**: lists databases the Azure CLI login can see (Resource Graph, metadata only), with suggested aliases and environments inferred from names (`prd` → production, `alp`/`stg` → staging). Saved with Entra ID, so nothing secret is stored. Production entries are unticked by default.
- Saved connections also live in the sidebar, grouped by environment, so switching is one click from anywhere.
- Secrets live in the OS keychain. Connection metadata lives in `~/.config/culprit/connections.toml`, shared with the CLI.

### Connected header

One line that answers "what am I looking at, and can I trust it":
`● shop-staging · SQL Server 2022 (Azure SQL Database) · Query Store on, 7 days · read-only · scanned 12 s ago ↻`
Missing capabilities show as a quiet chip ("3 checks locked") that opens Doctor.

### Workload window

Query Store keeps history by time, so Dashboard, Findings, Queries and Schema carry a **Last hour / Last 6 hours / Last 24 hours / Last 7 days / Last 30 days** picker in the top bar (`W` cycles it). The choice is remembered per connection, and every window has its own cached report: switching shows the last result for that window instantly (never another window's numbers) while a fresh scan runs. When the store holds less history than asked for, the bar says so ("Query Store holds 13 days"). Cumulative sources (`pg_stat_statements`, the SQL Server plan cache) show "since <reset>" instead of a picker. CLI: `culprit scan <conn> --window 6h` (days, hours or minutes: `6h`, `90m`, `2d`, `1d12h`, up to 90 days), or `--window-days 1|7|30`; `dashboard` and `waits` take the same flags to read that window's latest scan. Query Store keeps history in intervals (often an hour), so a window starts at the start of the interval its edge falls in, and the workload says how many hours it actually covers.

### Dashboard

One database at a glance, and where a connection opens. Not a wall of charts: a calm grid of fourteen tiles, each answering one question in a plain sentence first (*"Database time is up 2.3× in the last day compared with the 6 days before."*), then a small honest picture, then one quiet line on what the numbers cover. Clicking a tile (or Enter on it) opens the view that explains it.

- **Headline.** The most urgent tile's sentence, with a dot in its colour, and chips for every tile that needs a look (bad first, then by urgency: right now, database time, findings, query statistics, deadlocks, health, waits, plans, indexes). When nothing does: *"Nothing needs a look right now."* Below it, what the numbers cover (*"Query Store, the last 7 days · scanned 2 min ago"*).
- **Tiles.** *Database time*: per day (or measured totals under 20 hours), runs, CPU share, the step chart with plan flags, brush to compare, **What changed?**, and the queries that switched plans. *Findings*: counts by severity as one bar and the top three. *Top queries*: five rows (verb, table, predicate, trend line, share). *Where time goes*: the wait mix as one bar and a legend. *Right now*: running, blocked and idle-in-transaction sessions, a five-minute activity line, the blocking chain's sentence; read every 5 s, *"Updated 3 s ago"*, paused while the window is hidden. *Health*: every signal as a coloured square, the worst named. *Deadlocks*: ticks across the period and the patterns; Postgres shows its counter. *Indexes*: to add (with the share of database time behind them) and to clean up (with the space). *Plans*: captured plans as squares, the flagged ones amber, the commonest problems. *Query statistics*: what the source covers, or the one statement that turns it on (copy, open in the editor, re-scan).
- **Honest everywhere.** Every engine gets every tile. When an engine, login or scan can't show a signal, the tile is *unavailable* and says why in the engine's words (*"PostgreSQL keeps no history of what sessions wait on."*, *"MariaDB isn't counting waits: performance_schema is off"*) with the fix where there is one. Without query statistics, turning them on leads the page and the tiles that need them sink to the end.
- **Beyond time: storage, connections, replication, failing queries.** Four small tiles under the health row. *Storage* leads with the slowest file by what it's waited on for (data reads, log writes; "under 0.01 ms" rather than "0.00"). *Connections* says whether connections are reused or opened per request, and how many of the allowed are in use. *Replication* lists replicas and slots worst first, and says plainly when there's nothing to fall behind. *Failing queries* ranks statements by how often their runs fail or are cancelled; PostgreSQL, which keeps no per-statement error count, says so. Each leads with its check's finding when there is one, so the tile and the finding never disagree.
- **Same words as the CLI.** The tiles come from `culprit_checks::dashboard`, the code behind `culprit dashboard` and the MCP `dashboard` tool; only *Right now* is polled by the app (it words its tile identically).
- **Layout.** Twelve columns from ~880 px of content (a 1180 px window gets the full layout), two below, one on narrow panes; container queries, so it follows the content area. Light and dark, tabular numbers, no chart library, skeleton tiles in the same places while loading, reduced motion respected. The view is code-split; its skeleton is in the main bundle.
- **Keyboard.** `j` or `↓` steps into the grid; arrows move to the nearest tile in that direction, `j`/`k` and Home/End in reading order; Enter opens; `v` opens What changed; `2` is the ranked findings.

Why it leads instead of the findings list: the list answers "what should I fix", the dashboard answers "is anything wrong right now, and where should I look", which is the first question on opening a connection, and the list can't show blocking, deadlocks, a jump in database time or statistics that stopped recording. The findings are still the top of the dashboard's Findings tile and one key away.

### Findings (formerly Overview)

- Headline sentence: *"14 opportunities. The top 3 account for 38 % of database time over the last 7 days."*
- Vital signs strip: database time per day, what the database waits on (CPU / IO / locks / memory / network), size, data window.
- Filter chips: All · Indexes · Queries · Maintenance · Config · Health, with counts.
- Ranked list. Each row: impact bar, severity dot, human headline, subject (table / query), confidence.
- Detail pane on the right; the first finding is selected on open. `j`/`k` move, `Enter` focuses detail.
- Below the vitals, **database time per hour** across the window (a step chart; hover for each bucket's time, runs and range). Query Store's intervals on SQL Server; for sources with running totals only (`pg_stat_statements`, `performance_schema`, the plan cache) the buckets are the gaps between Culprit's own scans, so a trend appears from the second scan on and the chart says "between N scans".
- Findings whose queries stopped running (silent for 20× their usual interval, at least an hour, before the window ends) sink into a dimmed **Stopped running** group with "Last ran 7 h ago". A multi-day window otherwise keeps ranking yesterday's already-fixed problem first.

### Where time goes (waits)

The Findings view's middle card always answers "where does the time go", on every engine: the biggest category in plain words, a bar you can click by segment, and a legend. When the scan recorded no waits (PostgreSQL without `track_io_timing`) it says "Not tracked" and offers a live sample instead of disappearing. Under it, one calm health line names the signal that most needs a look, or says all is healthy.

Clicking either opens one panel with three tabs (`1`/`2`/`3`, `←`/`→`, `Esc`):

- **Where time goes:** categories (CPU, disk reads, row access, locks, in-memory contention, log writes, sending results, parallel queries, memory grants, compiling plans, everything else), each with what it means, one sentence about this database ("3 queries return 50K+ rows per call"), and the queries behind it with their share and their wait per run. A row opens the query. Categories an engine can't attribute say so.
- **Right now:** a 10/30/60-second sample. SQL Server and MySQL difference two readings of their wait counters; PostgreSQL samples busy sessions every second. It runs in the background (close the panel; it keeps going) and is compared with the scan's usual mix ("+64 pts vs history").
- **Health:** pressure signals as tiles, each with when it was measured (now / since start) and what healthy looks like in words, the oldest open transaction, and on Azure SQL Database the last hour of CPU, data IO, log writes and workers against the tier's limit (gridlines, dashed limit, peak, cursor readout).

Per engine: SQL Server attributes every Query Store wait category to queries; MySQL attributes lock time (table, metadata and InnoDB row locks) and reports the rest server-wide since startup; PostgreSQL keeps no wait history, so history is per-statement disk-read time plus WAL time when they're timed, and "right now" is session sampling. Short windows are never stretched into per-day or per-hour rates.

### Finding detail (the heart of the product)

```
INDEXES · high confidence · mssql.missing-index:dbo.Orders(CustomerId)
Orders are looked up by customer without a matching index
What's happening   plain-language paragraph
Impact             share of DB time · executions/day · avg duration · reads/execution
Evidence           affected queries (click → Queries) · plan excerpt with the hot operator highlighted
Fix                script, production-safe variant (ONLINE / CONCURRENTLY), cost of the fix
Verify             how to confirm: re-scan after deploy, what-if plan, run on alpha
[Copy fix c] [Copy for agent a] [Open in editor e] [Dismiss d]
```

**Copy for agent** produces a self-contained markdown brief (problem, evidence, fix, verification) to paste into a coding agent, which then writes the migration in the codebase.

### Queries

- Dense table: share of DB time (bar), total time, executions, average, max, rows/exec, reads/exec, CPU, plan count. Sort by any column, `/` to filter.
- A **Trend** sparkline per query: average duration per run across the window, red when the `regression` check flagged it.
- Query detail: numbers, an **Over time** chart (average per run as a line, runs as bars, dashed markers where a new plan first ran), linked findings, formatted SQL and the plan.
- SQL Server: plan history from Query Store (several plans for one query is the regression story). Postgres: generic plan via `EXPLAIN (GENERIC_PLAN)` on 16+.

### What changed and runtime spread

"It was fast yesterday" is the most common performance question, so comparing two periods is one gesture, not a report to configure.

- **Charts with context.** Every time chart (Overview's database time, a query's Over time) has value gridlines, local-time ticks, a peak marker, a readout that follows the pointer or the arrow keys, and flags where a plan first ran. A partial first bucket (data starting mid-bucket) is hatched and drawn dashed instead of reading as a cliff. Rates are per hour, or per minute when buckets are shorter than an hour.
- **Compare by brushing.** Drag across the Overview chart (or Shift+arrows, then Enter) to compare that range with the same length of time before it. `V`, the **What changed?** button, presets (last hour, 6 h, 24 h), a **Since** time (a deploy) or a plan flag's click all open the same panel in the detail pane.
- **The answer is a list of sentences, not a grid.** Groups: Got slower, New, Got faster, Stopped running, then Not comparable and Too few runs, with About the same folded away. Each row: verb · table · key predicate, a sentence ("Got 4.1× slower at Tue 14:05, when SQL Server switched from plan 815 to plan 685"), a before → after dumbbell on one log scale for every row, and a plan chip. Opening a row shows runs, average, fastest, slowest and database time per period, what differs between the plans, the SQL, and links to Queries and the editor.
- **Rigor that doesn't need explaining.** Averages are weighted by runs; a query needs 10 runs in each period and a 1.5× change (and at least 1 ms per run) to count. A missing period is never zero: a query Culprit's earlier scans didn't track is *Not comparable*, with the reason. Periods snap to Query Store intervals or to the nearest scans, and the panel shows the snapped periods and how much of the baseline the source covers. Short periods report the totals measured, never a per-hour extrapolation.
- **Every engine answers.** SQL Server compares Query Store intervals and names both plans, with what differs between them (Key Lookup → Clustered Index Scan). Postgres and MySQL compare Culprit's own scans (at least three) and notice a plan change from the plan shapes those scans captured.
- **Runtime spread** (Queries → Summary): one lane per plan on a shared log scale, whisker from the fastest to the slowest run, a band of one standard deviation, a dot at the average or the median, and each plan's compiled parameter values (SQL Server). SQL Server 2022's parameter-sensitive plan variants get their own lanes; MySQL adds its run-time histogram and p95/p99 marks; Postgres shows one lane with fastest, average, slowest and spread, and says that's all it keeps.
- **Parameter sensitivity** (`parameter-sensitive`): two plans used side by side where one is 5× slower (SQL Server), or a histogram whose slowest 1 in 100 runs take 10× the typical run (MySQL). The fix is a ladder to copy, lightest first: force the fast plan, RECOMPILE for this statement only, parameter-sensitive plan optimization, OPTIMIZE FOR UNKNOWN. Postgres's single-plan variance stays `unstable-query`, which shares the ladders.
- **Honest numbers everywhere.** Under 20 hours of history, Queries, Overview and finding cards show runs *in the measured span*, not per day.
- **What changed around it.** Every time chart has a change lane under it: marks (a deploy, a migration: `culprit mark`, the app's **Mark** button, the `mark` MCP tool), tables, columns, indexes, procedures and views, settings, upgrades, restarts and cloud throttling (shaded on the plot). Times are exact where the database dates the change (SQL Server `sys.objects`, MySQL `CREATE_TIME`/`LAST_ALTERED`/`variables_info`, Postgres configuration reloads and, with `track_commit_timestamp`, index builds) and otherwise a bracket between the two scans that noticed it; each scan compares itself with the previous one and keeps the result next to its report. Clicking a marker compares since then. In What changed, a strip shows both periods with their changes, **Around then** lists them oldest first with how many queries each may explain, and each row carries its likeliest explanation ("Index ix_orders_status was dropped from public.orders"), ranked by how directly it touches the query (its table, its procedure) and how close it is to the moment the query changed; statistics updates only appear when they precede a plan change, throttling only explains slower runs with the same plan.
- **Cloud limits.** For Amazon RDS/Aurora and Azure SQL Database or Azure Database for PostgreSQL/MySQL, Health shows what the cloud measured (CloudWatch through `aws`, Azure Monitor through `az`, read-only, the person's own sign-in): CPU against a burstable class's baseline, CPU credits, IOPS against the provisioned limit, storage and EBS burst balances, disk latency, DB load against vCPUs, the tier's CPU/data IO/log write/worker percentages, each against its limit with episodes shaded and listed ("CPU credits ran out … held to its 20% CPU baseline"). Fetches are cached a few minutes per resolution; the CLI is `culprit cloud`, agents use `cloud_metrics`.
- The Queries table's Query column reads verb · table · key predicate; Max, Rows/run and Reads/run step aside on narrow panes (then the Trend column), so the table never scrolls sideways.

### Plan viewer

An indented tree, read top-down like a profiler call tree, instead of a box-and-arrow graph. It scales to large plans and is scannable.

- Each row: operator, object, estimated → actual rows with a misestimate badge (`×4,100 under`), self cost bar coloured by heat.
- The hot path is expanded and marked; cold subtrees are collapsed.
- Warnings are chips on the row: implicit conversion, spill, key lookup, no join predicate, missing index.
- Selecting a node shows all its properties. A raw view shows the original XML / JSON.

### Plan insights

Every plan Culprit shows (Query Store, plan cache, Explain, Analyze, imported files) runs through one rules engine, `culprit_core::insights`: a pure function of the plan, so the diagram, the Operations table, findings, the CLI and the MCP server all say the same thing.

- **What an insight is.** One plain sentence ("Looks up 20K rows in shop.orders one at a time"), the operators it is about, the share of the plan's work at those operators (the ranking key after severity), why it matters with the plan's own numbers, a fix (SQL to copy where there is a statement; Culprit never runs it) and a way to verify the fix worked.
- **32 rules across the three engines.** SQL Server: key/RID lookups, implicit conversions, the optimizer's index hints, eager index spools, scalar UDFs, table variables, blocked parallelism, thread skew, spills, unused or waited-for memory grants, many-to-many merges, remote queries, missing statistics, optimizer timeouts, the legacy estimator, slow compiles. Postgres: heap fetches on index-only scans, lossy bitmaps, workers that didn't start, JIT overhead, per-row SubPlans, spills (sort, hash batches, hash aggregate). MySQL: join buffers, temporary-table grouping with filesort, dependent subqueries, ignored indexes. All engines: scans that throw most rows away, functions or casts around filtered columns, leading-wildcard LIKE, NOT IN, misestimates, repeated nested loops, big sorts, cross joins.
- **Honest coverage.** When a plan can't show something (an estimated plan has no actual rows; MySQL records no memory grants), the rules that need it stay silent and one sentence under the list says so, so "no problems found" is never a false clean bill.
- **No echoes.** One cause is reported once: a key lookup owns its covering-index fix (the optimizer's hint for the same index is dropped), NOT IN owns its spool and repeated loop, a function on a column owns the scan it causes, a cross join isn't also a loop problem.
- **Where it shows.** An Insights list above every plan (severity, area icon, impact bar; expanding one shows why, the fix with Copy, the verify step, and selects the operator); a numbered severity badge on the operator's card in the diagram and its row in the tree; an Operations view (sortable table of every operator: self and inclusive share, estimated vs actual rows with an "off by" ratio, runs, rows read, time, issues; filter and keyboard navigation); and the operator panel lists the insights about the selected operator.
- **Findings.** The `plan-insights` check turns medium and high insights on queries with at least 1% of database time into findings (one per problem and table, merged across queries; severity capped by the query's share), leaving to other checks what they already report (implicit conversions, spills, cross joins, missing statistics, filtered scans, missing indexes). A function or leading wildcard on the scanned column replaces filtered-scan's index advice for that query, since that index wouldn't be used until the query changes.

**Diagram.** Operator icons (seek, scan, lookup, loop/hash/merge join, sort, aggregate, spool, parallelism, compute, filter, limit), compact cards with the operator's share as a bar along the bottom edge, auto-fit and centered on load with a Fit button, pass-through wrappers (MySQL's query block, a 0% Compute Scalar) folded into the edge label, row counts in pills beside the edges and arrowheads for data flow. **Operator panel:** what it does (seek and filter conditions), rows (estimated, actual with a ratio badge, runs, read, thrown away by the filter), cost (self and inclusive share, optimizer units, CPU/IO split), warnings, then every raw property collapsed and searchable with readable numbers.

**Query detail** has Summary, Plan and History tabs (P toggles the plan; the last tab is remembered). Summary shows the top plan insight as a teaser that opens the plan.

### Plan compare and sharing

Two plans of the same statement (two Query Store plans, before and after an index, staging and production) side by side, explained rather than drawn:

- **Verdict first.** A pill ("151× costlier", "2.0× faster", by actual time when both plans ran, else estimated cost) and one sentence that says why: "Plan 69 is estimated 151× more expensive because it scans customers (88% of its cost) where Plan 68 seeks ix_customers_email."
- **What changed**: access, operator, added/removed branches, cost shifts, row changes, warnings; J/K step through them and select the step in both diagrams, centred like SSMS's compare.
- **Why**: the conditions the optimizer saw that differ, in plain language: estimator version (compatibility level), compiled parameter values, row estimates (with stale statistics from the connection's latest scan), indexes per table, type conversions that stop a seek, parallelism, memory grants and spills. Each engine gets the scenarios its plans carry evidence for.
- **Properties**: the selected step and its match, grouped (what it does, rows, cost, warnings, engine properties), with ≠ on every difference and identical engine properties folded away. Database names are ignored so prod and test plans line up.
- Diagrams tint identical regions alike on both sides; changed steps are dashed amber, steps only one plan has are dashed grey. Zoom and scroll move together unless Sync is off.

**Share** (plans, comparisons, findings): copy Markdown for a pull request, save a self-contained HTML page (inline SVG diagram, light and dark, prints cleanly, no scripts or network), or save the plan's own text (.sqlplan opens in SSMS). "Redact values" is on by default and remembered; exports never include connection names, hosts or credentials. **Verify fix** on a finding re-scans and answers in place: fixed, still present (with why, e.g. the window still holds earlier runs), or can't tell, plus any new findings.

### Live

- A thin status strip leads: a live dot with freshness (*"Updated 2 s ago"*, *"Paused while hidden"*; polling stops when the window is hidden or paused), a five-minute sparkline of active and blocked sessions, and a Sessions / Deadlocks switch.
- Active sessions (idle hidden by default), long transactions, idle-in-transaction (amber left stripe). Auto-refresh every 2.5 s, space to pause. Kill/cancel exists only on non-production connections.

#### Blocking chains

- Blocking is a tree, not a banner. Each chain is a calm card: a severity dot, one sentence (*"Session 57 has been idle in an open transaction for 12 min; 9 sessions wait behind it (an exclusive lock on shop.orders)."*), facts (transaction age, waiters, longest wait, the engine's wait type), what it usually means and what to do (two lines, *More* for the rest), and the tree of who waits behind whom.
- Lock names are words: `LCK_M_U` → "an update lock", MySQL `X,REC_NOT_GAP` → "an exclusive row lock", a Postgres wait on a transaction id → "a row lock". The raw wait type stays visible as a small code chip.
- Head kinds: idle in an open transaction (the classic: missing COMMIT, an error path, slow work inside the transaction), running while holding locks, idle but holding locks, not visible to this login. The advice is engine-specific (`SET XACT_ABORT ON`, `idle_in_transaction_session_timeout`, `innodb_lock_wait_timeout`).
- The head row offers *End session* (and *Cancel its statement* where the engine can) with an inline confirm, on non-production only. Rows are focusable; arrow keys move, Enter (or a click) highlights the session in the table.
- Lock details are read only when someone waits (one extra query): SQL Server `dm_os_waiting_tasks` + `dm_tran_locks` + partitions; Postgres `pg_locks` (row waits resolve to the table through the waiter's tuple lock); MySQL `performance_schema.data_lock_waits` + `data_locks`. MySQL idle heads show their last statement from `events_statements_current`.

#### Deadlocks

- Sources: SQL Server and Managed Instance read system_health (its event files *and* the ring buffer, merged, because files are written with a delay); MySQL reads the LATEST DETECTED DEADLOCK section of `SHOW ENGINE INNODB STATUS` (keeps only table, index and lock mode; the row contents InnoDB prints are dropped); Postgres only counts them (`pg_stat_database.deadlocks`), so the view shows the counter and asks for the log lines. Azure SQL Database reads its deadlock telemetry (`sys.fn_xe_telemetry_blob_target_read_file('dl', …)`, `database_xml_deadlock_report` events), which a database user can read where system_health is closed to it; graphs there name the database by its physical name, so events for other databases on the logical server are dropped by the event's `database_name`, and the processes carry the logical name. When the telemetry can't be read, the note says why and points at Azure Monitor.
- Paste (⌘V) or drop any of the three formats (SQL Server .xdl XML with DTDs refused, InnoDB status, Postgres `deadlock detected` log lines) to have it explained.
- Each deadlock gets a pattern (key lookup, read-then-update, opposite order, lock escalation, parallelism, lock cycle), a headline that names the tables and the victim, an exact story (*"Session 75 held shop.customers and wanted shop.orders; session 73 held shop.orders and wanted shop.customers."*), numbered fixes with copy-only SQL and links to the same fix as an existing finding (e.g. RCSI), and both statements.
- The graph: processes on the sides, locks in the middle; solid lines hold, dashed arrows want; the rolled-back process is tinted. Hover or focus a box to follow its locks.
- Every scan records a day of deadlocks; the `deadlocks` check groups them by pattern and tables: *"Two queries deadlocked 6 times in the last day (key-lookup deadlock on shop.customers)"*, leading with the primary fix and offering RCSI as the alternative.

### Actual runs and live progress

What a query really did, never by running it behind your back.

- **Last run** (Queries → a query → Plan → *Last run*). SQL Server 2019+ keeps the actual plan of each cached query's latest run when `LAST_QUERY_PLAN_STATS` is on; Culprit reads it from `sys.dm_exec_query_plan_stats` and shows real rows next to the estimates. That plan carries row counts only (no per-step time), and the view says so. When the setting is off, a calm card explains what it unlocks and shows the copy-only `ALTER DATABASE SCOPED CONFIGURATION` (also a `last-actual-plans` finding). Postgres and MySQL keep no actual plans; the card says so and offers *Measure it once* when the recorded text is runnable, or *Open in editor* to fill in real values.
- **Measured runs** (Editor *Analyze*, *Measure it once*): SQL Server runs the statement inside a transaction that is always rolled back (`SET STATISTICS XML`), Postgres runs `EXPLAIN (ANALYZE, BUFFERS)` inside a read-only transaction on production and a rolled-back one elsewhere, MySQL runs `EXPLAIN ANALYZE` on reads only. On production only statements the classifier proves read are measured.
- **Every actual plan reads by time, not cost.** When operators carry timing, shares, heat and "Where the 54 ms went" use each operator's *own* time (its inputs taken out: row-mode and Postgres/MySQL times include inputs, SQL Server batch mode doesn't, untimed operators such as Compute Scalar count as their inputs). The receipt above the diagram gives elapsed, CPU, reads (from disk), rows, memory used vs granted, and clickable callouts: the step that took the most time, where a row estimate first goes wrong ("Expected 288 rows, got 270K at Index Seek on support_tickets"), a worker doing most of the work, and spills.
- **Live progress** (Live → click or Enter on a running session). SQL Server: per-operator progress from `sys.dm_exec_query_profiles` and the in-flight plan from `sys.dm_exec_query_statistics_xml`, drawn on the plan (progress strip per card, done ticks, dimmed waiting steps, a ring on the step doing the work, "still reading its input" for sorts and hashes), an overall percentage weighted by estimated cost and labelled as such, and SQL Server's own `percent_complete` for the commands that report it. Lightweight profiling leaves the open/close timestamps at zero, so state comes from rows, finished scans and re-executions. Postgres: elapsed time, waits in words, the estimated plan of the running text, and `pg_stat_progress_*` for CREATE INDEX, VACUUM, ANALYZE, CLUSTER and COPY. MySQL: elapsed time and the plan MySQL is actually executing (`EXPLAIN FOR CONNECTION`), plus stage progress when performance_schema stage events are on. The panel refreshes every second only while it is visible, shows its freshness ("Live · just now"), pauses with one click, and closes with Esc.

### Schema

A health view, not a browser: tables by size with health badges (no clustered index, stale statistics, dead rows), indexes with reads vs writes, size, last use, unused / duplicate flags.

**Compare with…** shows environment drift against another connection of the same engine, from both latest scans: indexes missing on one side (with the `CREATE INDEX` to add it, labelled with the connection it is for; *Open in editor* only when that is the current connection), the same index under different names, tables and columns present on one side or typed differently, and settings and versions that differ. Indexes match by table, key columns, included columns, uniqueness and filter, never by name.

**Index advisor** (pinned at the top of the table list, `a`): one ranked plan for the whole database instead of per-table notes. Three tiles summarize it (to add, with the share of database time behind them; to clean up, with the space it frees; tables to look at), each a filter.

- **Add** — ordered by the database time of the queries that need each index, never by the optimizer's own impact guess. Candidates come from SQL Server's missing-index requests (merged when one index serves several, linked to the asking queries by query_hash on 2019+), from captured plans that read a whole table and filter on columns no index leads with (every engine), and from foreign keys with no index. Each opens to the reason in one sentence, the queries behind it (click to open them), the optimizer requests it merges, copy-only DDL (or the existing index to widen), the table's writes per day, and how to verify.
- **Clean up** — duplicates, indexes covered by a wider one, overlapping indexes that are barely read next to one with the same leading column, and indexes nothing reads. A covered or duplicate index whose keeper isn't read either is shown as unused, never as "keep the other one". The keys of both indexes are shown side by side with the shared prefix tinted. Fixes are reversible first: disable (SQL Server), invisible (MySQL); Postgres drops concurrently and keeps the definition in the finding.
- **Tables** — tables read in full over and over (Postgres seq_scan, MySQL rows read without an index), heaps whose reads follow forwarding pointers (SQL Server), and write-heavy tables carrying many indexes.

Keyboard: `j`/`k` move, `↵` opens, `c` copies the SQL, `o` opens the finding, `t` opens the table, `Esc` returns to the tables. Usage counters are stated with their window ("in the 6 days since the server last restarted"). What an engine can't tell the advisor is said in a calm note (Postgres and MySQL record no missing-index requests).

### Editor

CodeMirror (loaded lazily). Run (⌘↵, read-only transaction), Explain (⌘E, estimated plan, safe everywhere), Explain Analyze (⌘⇧E, executes the query, needs confirmation on production). Pin a plan and run again to compare.

### Command palette (⌘K)

Fuzzy search over views, findings, queries, tables, connections and actions (re-scan, switch connection, toggle theme, copy fix).

### SQL Server tuning, statement families and health

- **tuning-advice** (SQL Server): automatic tuning's plan-forcing recommendations still waiting (Active or Pending; a Verifying or Success one was applied and shows among the forced plans) ("SQL Server suggests forcing plan 815 for a read from orders (estimated 73% faster)") with the exact `sp_query_store_force_plan` to copy; forced plans that stopped applying (with the reason, e.g. NO_INDEX); forced plans that apply but scan a table an index leads with, the way the scan filters it (a plan forced before the index existed: Medium, with the `sp_query_store_unforce_plan` to copy); the other forced plans in one Low finding (by hand or by automatic tuning, since when, not run since); Azure SQL Database's CreateIndex and DropIndex recommendations, unless the missing-index finding for the same keys (which then says Azure recommends it too) or the unused- and duplicate-index findings already have them; one an existing index already covers (its keys and included columns) is skipped, since it predates that index and expires on its own, and one whose keys an index leads with but lacks its included columns says widening that index does the same; Query Store hints that fail; and automatic plan correction that is off while fixes wait, or set but not running.
- **query-family**: the same statement sent with different literals — Query Store queries sharing a query_hash (plan cache when Query Store is off), and pg_stat_statements entries that differ only in IN-list or VALUES length — plus plan caches filled with single-use plans. MySQL digests already merge literals; its counterpart is the digest table running out of room (config).
- **config** health per engine: Query Store near full / capture mode ALL / waits not captured, legacy cardinality estimation, unlimited MAXDOP (SQL Server); autovacuum off or too lazy for big tables, pg_stat_statements not tracking or evicting, shared_buffers at the 128 MB default (Postgres); performance_schema off, the digest consumer disabled, digests lost, statement text cut at 1,024 characters (MySQL).

### Signals beyond run time

Time and reads say where the work is; these say what else a statement, file or table costs. Each comes from a view the engine already keeps (statistics only, one cheap query each, a failure becomes a collector note), becomes a pure check with a stable id, and shows where the evidence lives: the query detail's **Beyond run time** block (one sentence first, then a row per signal with a dot), **Health → Storage / Replication**, the table header's facts in Schema, and an evidence table on server and index findings. Where an engine doesn't keep a number, the UI says so in one line instead of showing zero.

| Signal (check) | SQL Server | PostgreSQL | MySQL / MariaDB |
|---|---|---|---|
| Runs that fail or time out (`failing-query`) | Query Store aborted and exception runs, with the cancelled runs' average (30 s ≈ a client timeout) | not kept per statement | `SUM_ERRORS`, plus the error code recent failed runs raised (`1205 lock wait timeout`; never the message, which can hold row values) |
| Planning cost (`planning-time`) | Query Store compile time and count | `total_plan_time` with `pg_stat_statements.track_planning` | not kept |
| JIT overhead (`jit-overhead`) | — | `jit_*` columns, 15+ | — |
| Log volume per statement (`log-volume`) | `avg_log_bytes_used`, 2017+ | `wal_bytes` and full-page images, 13+ | not kept (no redo per digest) |
| Lock time per statement (`lock-time`) | Query Store lock waits | not kept; Live shows blocking | `SUM_LOCK_TIME` (row locks from MySQL 8.0.28) |
| Result size (`big-result`) | rows per run ≈ bytes from the plan's AvgRowSize | rows per run ≈ bytes from the plan width | rows sent per run |
| Storage latency (`slow-storage`) | `dm_io_virtual_file_stats` per file, this database and tempdb | `pg_stat_io` by object (16+), `pg_stat_database` read time before; needs `track_io_timing` | `file_summary_by_instance` per file (performance_schema) |
| Connection churn (`connection-churn`) | Logins/sec and pooled reuse (Connection Reset) | sessions, length and abandoned, 14+ | Connections, threads created, aborted connects |
| Hot rows (`hot-rows`, `last-page-insert`) | `dm_db_index_operational_stats` lock and latch waits per index; sequential keys and OPTIMIZE_FOR_SEQUENTIAL_KEY | — | per statement via lock time |
| Lock escalations (`lock-escalations`) | `dm_db_index_operational_stats` escalations per table (attempts are collected but not reported: the counter moves with SQL Server's background work), with the writes to it that change the most rows per run | never escalates | never escalates |
| Untrusted constraints (`untrusted-constraint`) | WITH NOCHECK or disabled FKs and checks | CHECK constraints added NOT VALID | not used by the optimizer |
| Missing histograms (`missing-histogram`) | — | — | heavy filters on columns with no leading index and no histogram (COLUMN_STATISTICS, `mysql.column_stats`), noting EXPLAIN's 10% / 33% guesses |
| HOT updates and bloat (`hot-updates`, `table-bloat`) | — | `n_tup_hot_upd`, `n_tup_newpage_upd` (16+), fillfactor; bloat estimated from pg_stats | — |
| Replica lag (`replica-lag`) | availability group redo and send queues, geo-replication lag | `pg_stat_replication`, standby replay lag, inactive slots keeping WAL | `SHOW REPLICA STATUS` (needs REPLICATION CLIENT) |

Wording follows the rest of the product: "A read from orders times out on every run, at about 15 s", "Planning takes almost all of the time of a read from orders", "Inserts into hot queue for the last page of pk_hot", "Reads from tempdb average 45 ms: storage is slow". Fixes are copy-only (an Extended Events session to capture errors, `OPTIMIZE_FOR_SEQUENTIAL_KEY`, `VALIDATE CONSTRAINT`, `UPDATE HISTOGRAM`, a fillfactor, `jit_above_cost`). A histogram finding whose columns an index finding already covers is dropped; lock time on statements that already fail with lock wait timeouts is left to `failing-query`.

### MariaDB

MariaDB runs on the MySQL provider and is first-class, not "MySQL with gaps" (the full matrix is in [parity.md](parity.md)):

- **Named as itself.** `VERSION()` decides the flavor once per connector. Badges show "Ma" in MariaDB's copper once a scan has named the server (`ReportSummary.product`); sentences, plan headers, coverage notes and fixes say MariaDB and use its syntax (`ALTER INDEX … IGNORED` from 10.6, `ANALYZE TABLE … PERSISTENT FOR COLUMNS`, `max_statement_time`). The editor uses the MariaSQL dialect.
- **No restart first.** MariaDB ships with performance_schema off. The unlock card leads with one `SET GLOBAL userstat = ON, slow_query_log = ON, log_output = 'FILE,TABLE', long_query_time = 0.1` (only the parts still needed), says connections opened earlier keep the old threshold and that it lasts until a restart unless also written to the configuration, and only then offers performance_schema with a restart.
- **The slow query log table is a real source.** Every logged run is grouped by fingerprint (literals out, lists collapsed), with exact percentiles and a histogram, a timeline from the runs' own timestamps, "What changed" for any period the log covers, lock time as the one wait it records, and the newest real statement of each shape as the sample to plan. It says what it misses: statements faster than `long_query_time`. With performance_schema on, the same log supplies samples for digests MariaDB kept no text for (matched by fingerprint).
- **Plans.** MariaDB's EXPLAIN/ANALYZE JSON has its own reader: sorts and temporary tables wrap their input, join buffers sit beside their table, rows are per loop (`loops` derived before 11.0, rows stand in for cost where there are no costs). ANALYZE adds time per operator, loops, rows, the share the filter kept, pages read and sort spills. Running statements show per-operator progress from `SHOW ANALYZE` (10.9+) and MariaDB's own PROGRESS for ALTER and LOAD.
- **Locks and waits.** Blocking reads `INNODB_LOCK_WAITS`; the deadlock parser reads MariaDB's WAITING FOR / CONFLICTING WITH layout and attributes locks by transaction id. Right now samples the process list when no wait instrument records anything. Index usage falls back to userstat, worded as "since userstat was turned on", because the server doesn't record when.

## Keyboard

| Key | Action |
|---|---|
| `1`–`7` | Switch view: Dashboard, Findings, Queries, Live, Schema, Editor, Plans |
| `⌘K` | Command palette |
| `j` / `k`, `↑` / `↓` | Move in lists |
| `/` | Filter current list |
| `v` (Dashboard, Findings) | What changed: compare recent vs baseline |
| `↑` `↓` `←` `→`, Enter (Dashboard) | Move between tiles, open one |
| `c` / `a` / `e` | Copy fix / copy for agent / open in editor |
| `⌘R` | Re-scan |
| `W` | Cycle the workload window: last 24 hours, 7 days, 30 days |
| `?` | Shortcut sheet |

## Safety model

| | Production | Staging / development / local |
|---|---|---|
| Session | Read-only, `LOCK_TIMEOUT` 2 s, statement timeout 10 s, `DEADLOCK_PRIORITY LOW` | Same defaults, can be relaxed |
| Explain (estimated) | Allowed | Allowed |
| Explain Analyze / Run | Explicit confirmation each time | Allowed |
| Kill session | Not available | Confirmation |
| Coding agents (`culprit mcp`) | Statistics and estimated plans by default; statements only with `query` access and a per-statement approval in the app | Same |
| Fix scripts | Copied, never applied | Copied, never applied (v1) |

Postgres sessions: `default_transaction_read_only=on`, `statement_timeout`, `lock_timeout`, `application_name=culprit`.
SQL Server sessions: `ApplicationIntent=ReadOnly`, `SET LOCK_TIMEOUT`, `SET DEADLOCK_PRIORITY LOW`, `READ UNCOMMITTED` for statistics views.
Expensive collectors (exact bloat, physical index stats) are opt-in.

### Coding agents

Agents connect through `culprit mcp` (stdio, so no port and no token). What they may do is a per-connection setting, `off | stats | query`, defaulting to `stats`: findings, statistics, schema, live sessions and estimated plans, nothing that runs application SQL. `query` adds `run_query`, which:

1. accepts only a single read-only statement (engine-aware classifier, `culprit_core::sql`);
2. queues an approval request in the data directory; the desktop app polls the queue while open (which also tells the server someone can answer), shows the agent, the connection, its environment and the exact SQL in a modal with Deny focused, and denies automatically after 60 seconds;
3. runs only on a decision whose hash matches the connection and SQL the agent sent, inside a transaction that is never committed (read-only on Postgres and MySQL), at most 200 rows, cancelled after 30 seconds.

Every tool call is appended to a private `audit.jsonl`, with the SQL text for `explain` and `run_query`. Tool descriptions and the server instructions tell agents that database-sourced text is data, not instructions.

## What it finds (v0.1)

| Check | Engines | Signal |
|---|---|---|
| `missing-index` | SQL Server | Missing-index DMVs, merged and de-duplicated against existing indexes, linked to the queries that asked for them |
| `filtered-scan` | both | Heavy queries whose plan reads a large table in full to keep a few rows; suggests the index from the filter's columns |
| `unindexed-fk` | both | Foreign key columns with no index starting with them |
| `unused-index` | both | Indexes with writes but no reads since stats reset, with uptime-based confidence; not before an index may be a week old (`indexes[].created_at_latest`: a scan saw it appear, or its table changed before the first scan) |
| `duplicate-index` | both | Identical or left-prefix-redundant indexes |
| `heavy-query` | both | Queries that dominate database time or read far more than they return |
| `chatty-query` | both | N+1 shape: very high executions, ~1 row each, tiny duration |
| `plan-warnings` | SQL Server | Implicit conversions, spills, key lookups, no join predicate in cached plans |
| `plan-regression` | SQL Server | Query Store: a newer plan is markedly slower than a previous one |
| `regression` | all | The query's average duration stepped up at least 2× (and 2 ms) at a point in the timeline and stayed up; says when, and the plan change if one coincides (then it replaces `plan-regression` for that query, with the force-plan fix) |
| `unstable-query` | SQL Server | Same plan, wildly different durations (parameter sensitivity) |
| `stale-stats` | both | Statistics with many modifications since last update |
| `heap-table` | SQL Server | Large tables without a clustered index |
| `dead-rows` | Postgres | Tables with many dead tuples and lagging autovacuum |
| `xid-age` | Postgres | Transaction ID age approaching wraparound |
| `temp-spill` | Postgres | Queries writing temp files (work_mem) |
| `config` | both | Query Store off or read-only, stale compatibility level, `random_page_cost` on SSD storage, missing `pg_stat_statements` |

## Architecture

```
crates/
  culprit-core       model (Snapshot, Plan, Finding), Check trait, ranking — no I/O
  culprit-checks     every check, each a pure fn(&Snapshot) -> Vec<Finding>
  culprit-mssql      Provider for SQL Server (tiberius): probe, snapshot, explain, live
  culprit-postgres   Provider for Postgres (tokio-postgres): same contract
  culprit-engine     connection store (TOML + OS keychain), connection-string parsing,
                     provider registry, scan orchestration, snapshot storage,
                     agent access, approval queue, audit log
  culprit-mcp        MCP server for coding agents (rmcp, stdio) and `culprit agent init`
  culprit-cli        `culprit` binary
app/
  src/               React UI
  src-tauri/         Tauri shell; commands call culprit-engine
```

**Checks run on snapshots, never on live connections.** A snapshot is a normalized, serializable picture of the database (queries, plans, tables, indexes, foreign keys, waits, settings). This makes every check a pure, unit-testable function, lets analysis run offline, lets a snapshot be shared with someone who has no database access, and makes the app, the CLI and the MCP server produce identical results.

**Pluggable at the seams that matter.**

- `Provider` trait — one implementation per engine, selected by a registry keyed on engine, compiled in behind cargo features; an embedding app can override or add one at runtime with `Workspace::with_provider`.
- `Check` trait — metadata (`id`, `title`, `category`, `engines`, `requires` capabilities) plus `run(&Snapshot)`. Checks declare the capabilities they need, so the UI and CLI can explain exactly what is locked and why.
- Engine-specific data rides in the normalized model as optional fields, so cross-engine checks (`unused-index`, `unindexed-fk`) are written once.
- Checks plug in without forking: `culprit_checks::analyze_with(snapshot, &extra)` or `Workspace::with_check(Arc::new(MyCheck))` in an embedding app or CLI. Plugged-in checks are ranked, summarized and locked exactly like built-ins and can't take over a built-in id (`crates/culprit-checks/tests/plugin.rs`).
- A declarative check format (SQL + thresholds in TOML) comes after v0.1, once the trait has settled.

**One source of truth for types.** Rust model types derive `ts-rs`; the React app imports the generated TypeScript.

**Fast start-up.**

- Tauri 2, release profile with LTO; window background matches the theme so there is no white flash.
- React bundle kept small: no UI framework, hand-written CSS with design tokens, the editor and raw plan views code-split.
- The last snapshot per connection is stored on disk and rendered immediately on open; the fresh scan replaces it in place (stale-while-revalidate).
- Snapshot sections are collected concurrently.

## CLI (agent-first)

```
culprit conn add <name> --url <url> | --ado <ado> [--env production|staging|development|local] [--auth password|entra]
culprit conn list | test <name> | rm <name> | rename <old> <new>
culprit conn discover [--cloud azure|aws] [--match <text>]… [--save] [--include-production]
                                      Azure SQL + PostgreSQL/MySQL flexible servers (Resource Graph), RDS/Aurora
culprit conn import [--from ads|vscode|dbeaver|datagrip] [--file <path>]… [--match <text>]… [--dry-run]
                                      metadata only; never passwords or credential stores
culprit auth logout                   forget device-code Entra sign-ins
culprit dashboard <conn> [--scan] [--no-live]   the database at a glance: fourteen tiles, what needs a look first
culprit doctor <conn>                 server, capabilities, permissions, locked checks
culprit scan <conn>                   snapshot + findings (saved)
culprit findings [<conn>]             findings from the latest snapshot
culprit show <finding-id>             one finding in full, including the agent brief
culprit top <conn> [--by time|cpu|reads|executions] [-n 20]
culprit explain <conn> (--sql <sql> | --file <path> | --query-id <id>) [--analyze]
culprit live <conn> [--cancel <id> | --kill <id>]   actions refused on production
culprit drift <a> <b>                 indexes, columns, settings that differ between two connections (or report files)
culprit changes <conn> [--since <time> | --recent 6h] [--no-cloud]
                                      queries slower/faster/new/stopped, each with what changed around it
culprit mark <conn> <text…> [--at <time>] | --list | --remove <id>
                                      put a deploy or migration on the timeline
culprit events <conn> [--last 24h | --since <time>] [--no-cloud]
                                      marks, schema/index/settings changes, restarts, throttling
culprit cloud <conn> [--last 24h|7d|14d] [--refresh]
                                      CloudWatch / Azure Monitor limits and throttling episodes
culprit checks                        every check with id, engines, requires
culprit analyze <file>                re-run the checks on a saved snapshot or report, offline
culprit mcp [--connection <name>]…    MCP server on stdio for coding agents
culprit conn set <name> --agent off|stats|query
culprit audit [--limit N]             what agents did through `culprit mcp`
culprit agent init [--dir .] [--connection <name>]… [--no-mcp]
```

Conventions:

- `--format json|md|text`; JSON is the default when stdout is not a terminal.
- Stable IDs: `<check-id>:<subject>`, e.g. `unused-index:dbo.Orders.IX_Orders_CreatedAt`.
- Errors are JSON on stderr in JSON mode: `{"error":{"code":"…","message":"…","hint":"…"}}`.
- Exit codes: `0` success, `1` error, `3` findings at or above `--fail-on <severity>`.
- `--report <file>` on read-only commands and `culprit analyze <file>` let agents work from a saved or shared report without database access.
- `conn add --password-env VAR` reads the password from an environment variable at connect time (CI, agents); nothing is stored.

## Out of scope for v0.1

MySQL, applying fixes from the app, HypoPG what-if, prod ⇄ alpha pairing and plan diff, scheduled scans, declarative checks. The architecture leaves room for each.
