# Culprit — design

Culprit connects to a database and tells you, in plain language, what is making it slow and what to do about it. It ships as a desktop app (Tauri + React) and a CLI (`culprit`) built on the same Rust core, so a coding agent sees exactly what a person sees.

## Principles

1. **Connect → answers.** The first screen after connecting is a ranked list of opportunities, not a schema tree. A scan reads only statistics views and finishes in seconds.
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
| App developer | "What's the low-hanging fruit in alpha before we ship?" | Overview |
| On-call engineer | "The API is slow right now. What is the database doing?" | Live |
| Coding agent | "Did my migration or query make things worse? What should I index?" | `culprit scan --json`, `culprit explain` |

## Information architecture

```
Connections (start)
└─ Connected workspace
   ├─ Overview   ranked opportunities + finding detail      (1)
   ├─ Queries    workload explorer + query detail + plan     (2)
   ├─ Live       sessions, blocking chains, waits           (3)
   ├─ Schema     table and index health (not a browser)      (4)
   └─ Editor     SQL, explain, run, compare plans            (5)
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

Query Store keeps history by time, so Overview, Queries and Schema carry a **Last 24 hours / Last 7 days / Last 30 days** picker in the top bar (`W` cycles it). The choice is remembered per connection, and every window has its own cached report: switching shows the last result for that window instantly (never another window's numbers) while a fresh scan runs. When the store holds less history than asked for, the bar says so ("Query Store holds 13 days"). Cumulative sources (`pg_stat_statements`, the SQL Server plan cache) show "since <reset>" instead of a picker. CLI: `culprit scan <conn> --window-days 1|7|30`.

### Overview

- Headline sentence: *"14 opportunities. The top 3 account for 38 % of database time over the last 7 days."*
- Vital signs strip: database time per day, what the database waits on (CPU / IO / locks / memory / network), size, data window.
- Filter chips: All · Indexes · Queries · Maintenance · Config · Health, with counts.
- Ranked list. Each row: impact bar, severity dot, human headline, subject (table / query), confidence.
- Detail pane on the right; the first finding is selected on open. `j`/`k` move, `Enter` focuses detail.
- Below the vitals, **database time per hour** across the window (a step chart; hover for each bucket's time, runs and range). Query Store's intervals on SQL Server; for sources with running totals only (`pg_stat_statements`, `performance_schema`, the plan cache) the buckets are the gaps between Culprit's own scans, so a trend appears from the second scan on and the chart says "between N scans".
- Findings whose queries stopped running (silent for 20× their usual interval, at least an hour, before the window ends) sink into a dimmed **Stopped running** group with "Last ran 7 h ago". A multi-day window otherwise keeps ranking yesterday's already-fixed problem first.

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

### Plan viewer

An indented tree, read top-down like a profiler call tree, instead of a box-and-arrow graph. It scales to large plans and is scannable.

- Each row: operator, object, estimated → actual rows with a misestimate badge (`×4,100 under`), self cost bar coloured by heat.
- The hot path is expanded and marked; cold subtrees are collapsed.
- Warnings are chips on the row: implicit conversion, spill, key lookup, no join predicate, missing index.
- Selecting a node shows all its properties. A raw view shows the original XML / JSON.

### Live

- If anything is blocked, a banner leads: *"2 sessions are blocked by session 57 for 34 s."* with the blocking tree.
- Active sessions (idle hidden by default), long transactions, idle-in-transaction (Postgres).
- Wait breakdown. Auto-refresh every 2 s, space to pause. Kill/cancel exists only on non-production connections.

### Schema

A health view, not a browser: tables by size with health badges (no clustered index, stale statistics, dead rows), indexes with reads vs writes, size, last use, unused / duplicate flags.

**Compare with…** shows environment drift against another connection of the same engine, from both latest scans: indexes missing on one side (with the `CREATE INDEX` to add it, labelled with the connection it is for; *Open in editor* only when that is the current connection), the same index under different names, tables and columns present on one side or typed differently, and settings and versions that differ. Indexes match by table, key columns, included columns, uniqueness and filter, never by name.

### Editor

CodeMirror (loaded lazily). Run (⌘↵, read-only transaction), Explain (⌘E, estimated plan, safe everywhere), Explain Analyze (⌘⇧E, executes the query, needs confirmation on production). Pin a plan and run again to compare.

### Command palette (⌘K)

Fuzzy search over views, findings, queries, tables, connections and actions (re-scan, switch connection, toggle theme, copy fix).

## Keyboard

| Key | Action |
|---|---|
| `1`–`5` | Switch view |
| `⌘K` | Command palette |
| `j` / `k`, `↑` / `↓` | Move in lists |
| `/` | Filter current list |
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
| `unused-index` | both | Indexes with writes but no reads since stats reset, with uptime-based confidence |
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
culprit doctor <conn>                 server, capabilities, permissions, locked checks
culprit scan <conn>                   snapshot + findings (saved)
culprit findings [<conn>]             findings from the latest snapshot
culprit show <finding-id>             one finding in full, including the agent brief
culprit top <conn> [--by time|cpu|reads|executions] [-n 20]
culprit explain <conn> (--sql <sql> | --file <path> | --query-id <id>) [--analyze]
culprit live <conn> [--cancel <id> | --kill <id>]   actions refused on production
culprit drift <a> <b>                 indexes, columns, settings that differ between two connections (or report files)
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
