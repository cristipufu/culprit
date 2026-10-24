# Culprit guide

The reference for the Culprit desktop app and the `culprit` CLI: what it checks, every command, how coding agents use it, what it guarantees on production, and how to build it. For the overview, see the [README](../README.md). For database-specific setup, sources and verified features, see [Engine parity](parity.md).

Culprit reads the statistics, catalog or file evidence described by each provider, ranks findings by measured impact,
and gives each finding the evidence, fix and a way to verify it. Scans do not apply fixes or run the application's
queries. The editor runs what you ask with production confirmations. The desktop app, CLI and coding-agent tools
share one Rust core.

## Contents

- [What it finds](#what-it-finds)
- [Quick start](#quick-start)
- [Coding agents](#coding-agents)
- [Speed](#speed)
- [Extending](#extending)
- [Safety](#safety)
- [Building](#building)
- [Layout](#layout)

## What it finds

These are examples; `culprit checks` lists the current checks. [Engine parity](parity.md) explains provider applicability.

| Check | What it means |
|---|---|
| `missing-index` | Index suggestions from the optimizer, merged, de-duplicated against existing indexes and linked to the queries that asked |
| `filtered-scan` | Heavy queries that read a whole table to keep a few rows, with the index that would avoid it |
| `plan-warnings` | Type mismatches that block index seeks, spills to disk, missing join predicates, missing statistics |
| `chatty-query` | Repeated small queries that may indicate polling or an N+1 pattern; request-level evidence is needed to confirm the cause |
| `heavy-query` | Single statements that dominate database time, and what their plan spends it on |
| `plan-regression` | Query Store saw a query get much slower after its plan changed |
| `regression` | A query got much slower at a point in the window: when it started, by how much, and whether a plan change coincides |
| `unstable-query` | The same statement has widely varying durations; source evidence guides the possible causes |
| `temp-spill` | Sorts and hashes exceeding `work_mem` |
| `unindexed-fk` | Foreign keys with no index on the referencing columns |
| `unused-index` | Indexes maintained on every write but never read |
| `duplicate-index` | Identical or left-prefix-redundant indexes |
| `stale-stats` | Optimizer statistics far behind the data |
| `heap-table` | Large tables without a clustered index |
| `dead-rows` | Dead tuples and lagging autovacuum |
| `xid-age` | Transaction ID wraparound risk |
| `config` | Source-backed settings advice, such as unavailable statement statistics or disabled automatic statistics |

Trends use the source's recorded intervals or differences between Culprit's saved scans. The [provider guides](parity.md)
explain which periods can be compared. `culprit drift <a> <b>` (Schema → Compare with…) lists the schema differences
between two connections, with the SQL to close the gap.

`culprit checks` lists them with the capabilities each needs. Checks that can't run with your login show up as locked, with the exact grant or setting that unlocks them.

## Quick start

```sh
# Save a connection. Secrets go to the OS keychain; Azure SQL can use your `az login` instead of a password.
culprit conn add shop-staging --env staging \
  --url 'Server=tcp:shop-staging.database.windows.net,1433;Database=shop;Authentication=Active Directory Default'

culprit conn discover --match shop --match stg --save   # or find Azure SQL, PostgreSQL and MySQL databases your az login can see
culprit conn import --dry-run                  # or bring in connections from Azure Data Studio, VS Code, DBeaver, DataGrip
culprit conn add app --url db/production.sqlite3   # a SQLite 3 file, opened read-only
culprit conn set app --query-log log/production.log   # its framework's log, when it isn't found beside the file (docs/providers/sqlite.md)

culprit scan shop-staging                      # findings, ranked
culprit scan shop-staging --window 6h          # the same over the last 6 hours (90m, 2d, 1d12h; up to 90 days)
culprit dashboard shop-staging                 # the database at a glance: what needs a look first
culprit changes shop-staging --recent 2h      # what got slower, and what changed around it
culprit mark shop-staging deploy v1.4.2         # put a deploy on the timeline (from CI, after a migration)
culprit cloud shop-staging                     # RDS / Azure metrics against their limits, via your aws or az sign-in
culprit show shop-staging '<finding id>'       # evidence, fix, verification, and a brief for your coding agent
culprit top shop-staging --by cpu -n 10        # heaviest queries
culprit explain shop-staging --sql 'SELECT …'  # estimated plan, never executes (--with-index tests virtual indexes on Postgres)
culprit lint-sql shop-staging --file migrations/ --format sarif   # lint migration DDL offline against the snapshot (--write rewrites in place)
culprit plan show slow.sqlplan                 # read a plan from another tool: .sqlplan, EXPLAIN JSON or text, MySQL JSON or tree, SQLite EXPLAIN QUERY PLAN
culprit plan diff shop-staging:42:7 shop-staging:42:9   # compare two Query Store plans of query 42 (or two files)
culprit update --check                         # is there a newer release? `culprit update` installs it
```

Or open the desktop app: saved connections are listed in the sidebar, grouped by environment. A connection opens on its **dashboard**: one sentence that says what needs a look first, then fourteen tiles (database time and its trend, findings, top queries, where time goes, sessions right now, health, deadlocks, indexes, plans, storage, connections, replication, failing queries, query statistics), each a plain takeaway, a small picture and a click into the view that explains it. Every engine gets every tile; one it can't fill says why. The ranked findings are one key away (`2`). Pick the engine and paste its connection string, or pick **SQLite** and choose the file (then **Add query log** if its framework's log isn't found beside it), use **Find Azure databases** / **Find AWS databases**, or **Import from other tools** (names, hosts and user names only; passwords are asked for once), and the first scan appears in seconds. Reopening a connection shows its last report instantly while a fresh scan runs. Rename a connection's alias with the pencil on its card or `culprit conn rename`.

Entra ID connections use your `az login`. Without the Azure CLI (or when it's signed out), Culprit signs in with a device code instead: the app shows the code, the CLI prints it when you're at a terminal. Access tokens stay in memory, the refresh token in the OS keychain; `culprit auth logout` forgets it. Scripts and agents never block on a sign-in: they get an error with a hint.

On non-production connections, Live can cancel a session's statement or end the session (`culprit live <conn> --cancel <id>` / `--kill <id>`). Production connections refuse both, in the CLI and the app.

### Plans: paste, compare

The **Plans** view (`7`, also without a connection) draws any plan you paste (⌘V anywhere), drop or open: SQL Server .sqlplan or showplan XML, Postgres `EXPLAIN` as text or `FORMAT JSON` (psql output included), MySQL `EXPLAIN FORMAT=JSON` or `EXPLAIN ANALYZE`. **Compare** puts two plans side by side and says what changed in plain words ("Plan 9 scans orders where Plan 7 seeks ix_orders_customer"), with every changed, added or removed operator marked in both diagrams. On a query with several Query Store plans, **Compare** in the Queries view opens the fastest and the slowest plan; **Open in Plans** keeps any plan to save it (.sqlplan opens again in SSMS) or compare it with another environment's.

### Editor and saved queries

The **Editor** keeps your query tabs across restarts (⌘T new, ⌘⇧W close, ⌘⇧[ / ⌘⇧] switch), completes schemas, tables and columns as you type, read live from the connection (aliases included: after `o.` it offers only that table's columns, with their types), and lints DDL and migration statements inline against the latest snapshot with one-click quick fixes. ⌘↵ runs the selection or the whole tab, ⌘E shows the plan without running it, ⌘S saves the tab as a named query.

**Saved queries** live in the editor's side rail, the command palette (⌘K, "Run saved: …") and the CLI (`culprit saved list | add | show | run | rm`); each remembers the connection it was saved for and can run on any other.

**What runs without asking:** a statement Culprit's classifier proves only reads runs anywhere, in a read-only transaction (rolled back on SQL Server, which has none). Anything else commits; on a **production** connection only after a confirmation that names what it does ("This UPDATE has no WHERE clause, so it changes every row in the table"), and statements that touch every row or drop objects need the connection's name typed. In the CLI, `--yes-execute-on-production` is that confirmation. `culprit catalog <conn>` prints what completion sees.

Findings whose queries stopped running well before the end of the data window (relative to how often they used to run) move to a "Stopped running" group: usually a deploy changed or fixed them.

## Coding agents

Culprit is built to be driven by coding agents. Every command prints JSON when stdout isn't a terminal (or SARIF 2.1.0 with `--format sarif`), finding ids are stable, and connections are referenced by name, so agents never handle credentials.

**MCP server.** `culprit mcp` serves Culprit over the Model Context Protocol on stdio (no network port, no token). Tools: `list_connections`, `scan`, `dashboard`, `findings`, `finding`, `top_queries`, `query`, `query_plan`, `plan_insights`, `last_actual_plan`, `query_progress`, `what_changed`, `events`, `cloud_metrics`, `mark`, `explain`, `lint_sql`, `live`, `blocking`, `deadlocks`, `waits`, `schema`, `drift`, `index_advice`, `verify`, `export_finding`, `run_query`. All are read-only except `mark`, which adds a note to the connection's local timeline.

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

More in [agents.md](agents.md).

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
- Statements Culprit runs as reads must be exactly one statement that only reads, checked under every server setting that changes parsing; on SQL Server the server confirms it first by compiling the text without running it. For agents with `query` access on production, use a read-only login: see [agents.md](agents.md).
- Postgres sessions are read-only with statement and lock timeouts. SQL Server sessions use a short lock timeout, low deadlock priority and read-uncommitted reads of the statistics views.
- Estimated plans never execute anything. Reads run in a read-only transaction everywhere; on production, anything Culprit can't prove reads needs an explicit confirmation each time, and Explain Analyze only measures provable reads.
- Statement text from a database's own history is lexed as exactly one statement before it reaches `EXPLAIN` (MySQL plans go through prepared statements). TLS certificates are verified for every non-local host unless the connection string asks for less.
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

### Platforms

- **macOS.** `dev/build-local.sh` builds the app and the CLI and signs both with your Developer ID, so the keychain's "Always Allow" sticks across rebuilds.
- **Linux.** The desktop app needs WebKitGTK; on Debian and Ubuntu: `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev librsvg2-dev libayatana-appindicator3-dev libssl-dev libxdo-dev`. `bunx tauri build --bundles deb` makes a `.deb`. Passwords go to the Secret Service (GNOME Keyring, KWallet); on servers, containers and CI, which have none, pass passwords with `--password-env NAME` and set `CULPRIT_KEYCHAIN=off`. Configuration and reports follow the XDG directories (`~/.config/culprit`, `~/.local/share/culprit`).
- **Windows.** Passwords go to Windows Credential Manager, and configuration and reports to `%APPDATA%\culprit`. The Azure and AWS CLIs are run as `az.cmd` and `aws.exe`. `bunx tauri build --bundles msi` makes an installer. Shortcut labels read Ctrl where macOS shows ⌘.

CI (`.github/workflows/ci.yml`) checks formatting, runs clippy and the tests on Linux, macOS and Windows, and builds the desktop app on all three, keeping the unsigned bundles as build artifacts.

## Layout

```
crates/culprit-core      model: snapshots, plans, findings; Provider and Check contracts
crates/culprit-checks    every check, as a pure function of a snapshot
crates/culprit-mssql     SQL Server provider
crates/culprit-postgres  PostgreSQL provider and TimescaleDB integration
crates/culprit-yugabyte  Native YugabyteDB YSQL/DocDB provider
crates/culprit-mysql     MySQL and MariaDB provider
crates/culprit-sqlite    SQLite 3 file provider
crates/culprit-querylog  framework query log parser (Rails, Django, EF Core, Ecto, JSONL)
crates/culprit-engine    connection store, keychain, Entra tokens, scans, reports, agent approvals and audit log
crates/culprit-mcp       the MCP server for coding agents and `culprit agent init`
crates/culprit-cli       the `culprit` binary
app/                     React UI and the Tauri shell
docs/DESIGN.md           UX and functional design
docs/guide.md            this guide
```
