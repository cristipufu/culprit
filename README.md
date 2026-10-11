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

`culprit checks` lists them with the capabilities each needs. Checks that can't run with your login show up as locked, with the exact grant or setting that unlocks them.

## Quick start

```sh
# Save a connection. Secrets go to the OS keychain; Azure SQL can use your `az login` instead of a password.
culprit conn add shop-staging --env staging \
  --url 'Server=tcp:shop-staging.database.windows.net,1433;Database=shop;Authentication=Active Directory Default'

culprit conn discover --match shop --match stg --save   # or find Azure SQL databases your az login can see and save them

culprit scan shop-staging                      # findings, ranked
culprit show shop-staging '<finding id>'       # evidence, fix, verification, and a brief for your coding agent
culprit top shop-staging --by cpu -n 10        # heaviest queries
culprit explain shop-staging --sql 'SELECT …'  # estimated plan, never executes
```

Or open the desktop app: saved connections are listed in the sidebar, grouped by environment. Paste a connection string or use **Find Azure SQL databases**, and the first scan appears in seconds. Reopening a connection shows its last report instantly while a fresh scan runs. Rename a connection's alias with the pencil on its card or `culprit conn rename`.

Findings whose queries stopped running well before the end of the data window (relative to how often they used to run) move to a "Stopped running" group: usually a deploy changed or fixed them.

## For coding agents

Every command prints JSON when stdout isn't a terminal, finding ids are stable, and connections are referenced by name so agents never handle credentials. See [docs/agents.md](docs/agents.md).

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
crates/culprit-engine    connection store, keychain, Entra tokens, scans, reports
crates/culprit-cli       the `culprit` binary
app/                     React UI and the Tauri shell
docs/DESIGN.md           UX and functional design
```
