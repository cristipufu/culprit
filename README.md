# Culprit

Connect to a database and see, in plain language, what is slowing it down and what to do about it.

Culprit reads the statistics your database already keeps (Query Store, plan cache and DMVs on SQL Server; `pg_stat_statements` and the statistics views on Postgres), ranks what it finds by share of database time, and gives every finding the evidence, a production-safe fix and a way to verify it. It never runs your queries unless you ask, and never writes to the database.

It ships as a fast desktop app (Tauri + React) and a CLI built on the same Rust core, so a coding agent sees exactly what you see.

## What it finds

| Check | Engines | What it means |
|---|---|---|
| `missing-index` | SQL Server | Index suggestions from the optimizer, merged, de-duplicated against existing indexes and linked to the queries that asked |
| `filtered-scan` | both | Heavy queries that read a whole table to keep a few rows, with the index that would avoid it |
| `plan-warnings` | both | Type mismatches that block index seeks, spills to disk, missing join predicates, missing statistics |
| `chatty-query` | both | N+1 loops: tiny queries executed so often the round trips dominate |
| `heavy-query` | both | Single statements that dominate database time, and what their plan spends it on |
| `plan-regression` | SQL Server | Query Store saw a query get much slower after its plan changed |
| `regression` | all | A query got much slower at a point in the window: when it started, by how much, and whether a plan change coincides |
| `unstable-query` | both | Same plan, wildly different durations (parameter sensitivity, blocking) |
| `temp-spill` | Postgres | Sorts and hashes exceeding `work_mem` |
| `unindexed-fk` | both | Foreign keys with no index on the referencing columns |
| `unused-index` | both | Indexes maintained on every write but never read |
| `duplicate-index` | both | Identical or left-prefix-redundant indexes |
| `stale-stats` | both | Optimizer statistics far behind the data |
| `heap-table` | SQL Server | Large tables without a clustered index |
| `dead-rows` | Postgres | Dead tuples and lagging autovacuum |
| `xid-age` | Postgres | Transaction ID wraparound risk |
| `config` | both | Query Store off or full, RCSI off, auto statistics off, old compatibility level, `random_page_cost` on SSD storage |

Trends: Overview charts database time per hour and Queries draws a sparkline per query, from Query Store's intervals on SQL Server and from the differences between Culprit's own scans on Postgres and MySQL (which only keep running totals). `culprit drift <a> <b>` (Schema → Compare with…) lists the indexes, columns and settings that differ between two connections, with SQL to add a missing index.

`culprit checks` lists them with the capabilities each needs. Checks that can't run with your login show up as locked, with the exact grant or setting that unlocks them.

## Quick start

```sh
# Save a connection. Secrets go to the OS keychain; Azure SQL can use your `az login` instead of a password.
culprit conn add shop-staging --env staging \
  --url 'Server=tcp:shop-staging.database.windows.net,1433;Database=shop;Authentication=Active Directory Default'

culprit conn discover --match shop --match stg --save   # or find Azure SQL, PostgreSQL and MySQL databases your az login can see
culprit conn import --dry-run                  # or bring in connections from Azure Data Studio, VS Code, DBeaver, DataGrip

culprit scan shop-staging                      # findings, ranked
culprit show shop-staging '<finding id>'       # evidence, fix, verification, and a brief for your coding agent
culprit top shop-staging --by cpu -n 10        # heaviest queries
culprit explain shop-staging --sql 'SELECT …'  # estimated plan, never executes
culprit plan show slow.sqlplan                 # read a plan from another tool: .sqlplan, EXPLAIN JSON or text, MySQL JSON or tree
culprit plan diff shop-staging:42:7 shop-staging:42:9   # compare two Query Store plans of query 42 (or two files)
```

Or open the desktop app: saved connections are listed in the sidebar, grouped by environment. Paste a connection string, use **Find Azure databases** / **Find AWS databases**, or **Import from other tools** (names, hosts and user names only; passwords are asked for once), and the first scan appears in seconds. Reopening a connection shows its last report instantly while a fresh scan runs. Rename a connection's alias with the pencil on its card or `culprit conn rename`.

Entra ID connections use your `az login`. Without the Azure CLI (or when it's signed out), Culprit signs in with a device code instead: the app shows the code, the CLI prints it when you're at a terminal. Access tokens stay in memory, the refresh token in the OS keychain; `culprit auth logout` forgets it. Scripts and agents never block on a sign-in: they get an error with a hint.

On non-production connections, Live can cancel a session's statement or end the session (`culprit live <conn> --cancel <id>` / `--kill <id>`). Production connections refuse both, in the CLI and the app.

### Plans: paste, compare

The **Plans** view (⌘6, also without a connection) draws any plan you paste (⌘V anywhere), drop or open: SQL Server .sqlplan or showplan XML, Postgres `EXPLAIN` as text or `FORMAT JSON` (psql output included), MySQL `EXPLAIN FORMAT=JSON` or `EXPLAIN ANALYZE`. **Compare** puts two plans side by side and says what changed in plain words ("Plan 9 scans orders where Plan 7 seeks ix_orders_customer"), with every changed, added or removed operator marked in both diagrams. On a query with several Query Store plans, **Compare** in the Queries view opens the fastest and the slowest plan; **Open in Plans** keeps any plan to save it (.sqlplan opens again in SSMS) or compare it with another environment's.


Findings whose queries stopped running well before the end of the data window (relative to how often they used to run) move to a "Stopped running" group: usually a deploy changed or fixed them.

## Coding agents

Culprit is built to be driven by coding agents. Every command prints JSON when stdout isn't a terminal, finding ids are stable, and connections are referenced by name, so agents never handle credentials.

**MCP server.** `culprit mcp` serves Culprit over the Model Context Protocol on stdio (no network port, no token). Tools: `list_connections`, `scan`, `findings`, `finding`, `top_queries`, `query`, `query_plan`, `explain`, `live`, `schema`, `run_query`.

```sh
claude mcp add culprit -- culprit mcp                 # Claude Code
culprit mcp --connection shop-staging                 # expose only some connections
```

Cursor (`.cursor/mcp.json`), Codex (`~/.codex/config.toml`, `[mcp_servers.culprit]`) and other clients take the same command: `culprit` with the argument `mcp`.

**What agents may do** is set per connection, in the app (the "Agents" menu on each connection) or with `culprit conn set <name> --agent off|stats|query`:

| Access | Agents can |
|---|---|
| `off` | Nothing; the connection is hidden from them |
| `stats` (default) | Read findings, query statistics, schema, live sessions and estimated plans. Nothing runs your SQL |
| `query` | Also run one read-only statement at a time, each one only after you approve that exact text in the desktop app |

`run_query` refuses anything that isn't a single statement that only reads data. It runs inside a transaction that is never committed (read-only on Postgres and MySQL), returns at most 200 rows and is cancelled after 30 seconds. Approval requests show the agent's name, the connection and the exact SQL; Deny is the default, and they are denied automatically after 60 seconds or when the app isn't open.

**Audit log.** Every tool call is appended to `audit.jsonl` in Culprit's data directory (readable by your user only), with the SQL text for `explain` and `run_query`. `culprit audit` shows the latest entries.

**Set up a repository.** `culprit agent init` writes a short Culprit section into `AGENTS.md`, a Claude Code skill (`.claude/skills/culprit/SKILL.md`) and a `.mcp.json` entry, so agents in that repo use Culprit without being told. It's safe to re-run and writes no findings, host names or secrets; pass `--connection <name>` to name the connections the project uses.

More in [docs/agents.md](docs/agents.md).

## Speed

Measured on an Apple-silicon MacBook (macOS 26), release build, empty profile, three launches each, time from process start to the first on-screen window:

| | Launch → window (cold / warm / warm) | Physical memory after 4 s | App size |
|---|---|---|---|
| Culprit (Tauri) | 857 / 277 / 250 ms | ~99 MB across 4 processes | 8.9 MB |
| A popular native macOS database client | 1009 / 267 / 271 ms | ~44 MB in 1 process | 383 MB |

Memory is `phys_footprint` (what macOS charges the app), not RSS, which double-counts shared system frameworks. Culprit's own process is ~28 MB; the rest is the system WebKit that renders the UI (web content ~41 MB, GPU ~23 MB, networking ~7 MB), the fixed cost of a webview UI over a native one.

Reopening a connection renders its last report from disk immediately; a fresh SQL Server scan takes 0.1–0.5 s locally and 2.5–4.5 s against Azure SQL over the internet (1.4K queries, 30 days of Query Store). `CULPRIT_TRACE_STARTUP=1` prints the app's own window-shown time.

## Extending

- **Checks:** implement `culprit_core::Check` and pass it to `culprit_checks::analyze_with` or `Workspace::with_check`; it runs, ranks and locks like a built-in check.
- **Engines:** providers implement `culprit_core::Provider`; built-in ones are cargo features, and `Workspace::with_provider` plugs one in at runtime.

## Safety

- Scans read statistics and catalog views only.
- Postgres sessions are read-only with statement and lock timeouts. SQL Server sessions use a short lock timeout, low deadlock priority and read-uncommitted reads of the statistics views.
- Estimated plans never execute anything. `Run` and `Explain analyze` on a production connection need explicit confirmation each time.
- Coding agents (`culprit mcp`) read statistics and estimated plans by default. They run statements only on connections you opt in, one approved read-only statement at a time, and every call is audited.
- Fix scripts are shown and copied, never applied. They use `ONLINE = ON` / `CONCURRENTLY` where the server supports it.
- Connection metadata (`~/.config/culprit/connections.toml`) and reports (`~/.local/share/culprit/reports/`) are readable by your user only. `CULPRIT_HOME` puts both in one directory.

## Building

Requirements: Rust (stable), Bun.

```sh
cargo build --release -p culprit-cli          # target/release/culprit
cd app && bun install && bunx tauri build     # desktop app
cd app && bun run dev                         # UI in a browser, backed by fixtures/*.report.json
```

`cargo test --workspace` runs the unit tests. `dev/postgres` and `dev/mssql` contain reproducible demo databases with planted problems.

## Layout

```
crates/culprit-core      model: snapshots, plans, findings; Provider and Check contracts
crates/culprit-checks    every check, as a pure function of a snapshot
crates/culprit-mssql     SQL Server provider
crates/culprit-postgres  Postgres provider
crates/culprit-engine    connection store, keychain, Entra tokens, scans, reports, agent approvals and audit log
crates/culprit-mcp       the MCP server for coding agents and `culprit agent init`
crates/culprit-cli       the `culprit` binary
app/                     React UI and the Tauri shell
docs/DESIGN.md           UX and functional design
```
