<p align="center">
  <img src="docs/images/logo.svg" width="88" height="88" alt="Culprit SQL Studio">
</p>

<h1 align="center">Culprit SQL Studio</h1>

<p align="center">
  <b>Find out what's slowing your database down, and exactly how to fix it.</b>
</p>

<p align="center">
  A fast desktop app and CLI for SQL Server, PostgreSQL, MySQL and MariaDB, on macOS, Linux and Windows.<br>
  Plain-language answers for people. Safe, structured access for coding agents.
</p>

<p align="center">
  <img alt="SQL Server" src="https://img.shields.io/badge/SQL%20Server-CC2927?style=flat-square&logo=microsoftsqlserver&logoColor=white">
  <img alt="PostgreSQL" src="https://img.shields.io/badge/PostgreSQL-336791?style=flat-square&logo=postgresql&logoColor=white">
  <img alt="MySQL" src="https://img.shields.io/badge/MySQL-00758F?style=flat-square&logo=mysql&logoColor=white">
  <img alt="MariaDB" src="https://img.shields.io/badge/MariaDB-003545?style=flat-square&logo=mariadb&logoColor=white">
  <img alt="MCP server" src="https://img.shields.io/badge/MCP-ready-1b1a18?style=flat-square">
  <img alt="macOS, Linux and Windows" src="https://img.shields.io/badge/macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-1b1a18?style=flat-square">
  <a href="https://github.com/cristipufu/culprit-sql-studio/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/cristipufu/culprit-sql-studio/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-ffcf33?style=flat-square"></a>
</p>

<p align="center">
  <a href="#why-culprit">Why Culprit</a> ·
  <a href="#what-you-get">What you get</a> ·
  <a href="#built-for-coding-agents">Coding agents</a> ·
  <a href="#safe-on-production">Safety</a> ·
  <a href="#get-started">Get started</a> ·
  <a href="docs/guide.md">Guide</a>
</p>

<br>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/dashboard-dark.png">
  <img alt="Culprit's dashboard for a SQL Server database: database time over five hours with a plan change, a deploy mark and a throttling period on its timeline, 24 findings ranked by severity, the top queries, where time goes, a blocked session right now, health, deadlocks, index advice and plan insights" src="docs/images/dashboard-light.png">
</picture>

<br>

## Why Culprit

When a database gets slow, someone has to dig through Query Store, wait statistics, plan caches and execution plans to find out why. That takes an expert, an afternoon and a handful of tools that each show a piece of the picture.

Culprit does the digging in seconds. It reads the statistics your database already keeps, works out where the time goes, and tells you in one sentence what is wrong, what it costs and what to change:

> **A type mismatch on `customers.email` stops a read from `customers` from using an index.**<br>
> 54% of database time · **Fix:** send the parameter as `varchar`, the column's type · **Verify:** re-scan; the warning is gone from the plan.

<table>
<tr>
<td width="33%" valign="top">

**🎯 Answers, not raw numbers**

Every finding is ranked by its share of database time and comes with the evidence, a production-safe fix and a way to prove it worked.

</td>
<td width="33%" valign="top">

**🛡️ Safe on production**

Scans read statistics only and never run your queries. Anything that would change data on a production database asks first.

</td>
<td width="33%" valign="top">

**🤖 Built for coding agents**

An MCP server, JSON output everywhere and per-connection permissions let Claude Code, Cursor or Codex investigate with you. Every call is approved and audited.

</td>
</tr>
<tr>
<td valign="top">

**⚡ Fast and light**

A 14 MB app that opens in under a second and shows the last report instantly while a fresh scan runs in the background.

</td>
<td valign="top">

**🧭 One tool, four engines**

The same views, wording and workflow on SQL Server, PostgreSQL, MySQL and MariaDB. When an engine can't show something, Culprit says why in one sentence.

</td>
<td valign="top">

**🔌 Open and pluggable**

Add your own checks or database engines without forking. The app, the CLI and the agent tools share one Rust core.

</td>
</tr>
</table>

## What you get

### Findings you can act on

50 checks look for what actually costs time: type mismatches that block indexes, missing and unused indexes, N+1 query loops, plan regressions, parameter-sensitive plans, queries that fail or time out, spills to disk, hot rows, slow storage, connection storms, replication lag, stale statistics, risky settings and more. Each finding names the queries involved, shows their share of database time, and ends with a fix you copy (Culprit never applies it for you) and a way to verify it.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/findings-dark.png">
  <img alt="A finding: a type mismatch on customers.email stops an index seek, with its share of database time, the query as evidence, the expensive plan step, the fix and how to verify it" src="docs/images/findings-light.png">
</picture>

### Plans that explain themselves

Every captured plan comes with insights in plain words ("a type mismatch stops SQL Server from seeking an index"), a diagram that shows where the cost goes, and the operator's details one click away. 32 rules cover SQL Server, PostgreSQL and MySQL plans. Measure a run safely (writes are rolled back), and on SQL Server see the last actual plan without re-running the query or watch a long query's progress step by step.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/queries-dark.png">
  <img alt="The Queries view: queries ranked by database time with trend sparklines, and the selected query's plan with an insight, a cost bar and a plan diagram" src="docs/images/queries-light.png">
</picture>

### "What changed?" in one drag

Drag across the database-time chart to compare two periods. Culprit lists what got slower, what's new and what improved, and says why: "Got 293× slower at 5:00 PM, when SQL Server switched from plan 163 to plan 198." It also lines regressions up with everything else that changed around them: a dropped index, a new setting, a restart, a deploy your pipeline marked with `culprit mark`, or the cloud throttling the server (Amazon RDS CPU credits running out, Azure tier limits), read with your own `aws` or `az` sign-in.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/what-changed-dark.png">
  <img alt="What changed between two periods: one query got 293 times slower after a plan switch, seven new queries, with database time and runs before and after" src="docs/images/what-changed-light.png">
</picture>

### Compare any two plans

Paste or drop a plan from anywhere (SSMS `.sqlplan`, PostgreSQL `EXPLAIN`, MySQL `EXPLAIN FORMAT=JSON`) and compare it with another. The headline says what matters: "Plan 69 is estimated 151× more expensive because it scans customers where Plan 68 seeks ix_customers_email." Every changed step is marked in both diagrams.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/compare-dark.png">
  <img alt="Two SQL Server plans side by side, with a one-sentence explanation of why one costs 151 times more and a list of the changed steps" src="docs/images/compare-light.png">
</picture>

### Who is blocking whom, right now

Live sessions turn a wall of lock waits into one sentence and a tree: which session holds everyone up, why (here: a transaction left open by the application) and who waits behind it. Deadlocks get the same treatment, with the pattern named and the fix spelled out.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/live-dark.png">
  <img alt="Live sessions: session 71 has been idle in an open transaction for 22 seconds and two sessions wait behind it, shown as a blocking tree" src="docs/images/live-light.png">
</picture>

### And everything else, done properly

| Feature | What it does |
|---|---|
| **Index advisor** | Indexes to add, ranked by the database time of the queries that need them, plus duplicate, overlapping and unused indexes to clean up, with what each costs on writes |
| **Where time goes** | Wait statistics in plain words, traced back to the queries behind them, with a live sample to compare against the usual mix |
| **Health signals** | Memory, compiles, connections, row locks, storage latency per file, replication lag and cloud limits, each graded with a one-line takeaway |
| **SQL editor** | Tabs, autocomplete for schemas, tables and columns, saved queries, estimated and measured plans. Reads run read-only; production changes ask first |
| **Schema explorer** | Tables, sizes, indexes and how each index is used, plus drift between two environments with the SQL to close the gap |
| **Connections** | Paste any connection string, discover Azure and AWS databases with your existing CLI login, or import from Azure Data Studio, VS Code, DBeaver and DataGrip. Passwords stay in your OS keychain |
| **Keyboard first** | A command palette (⌘K) for every view, finding, query and action |

## Built for coding agents

Culprit is designed to be driven by an agent as comfortably as by a person. `culprit mcp` serves 25 tools over the Model Context Protocol (dashboard, findings, plans, insights, what changed, blocking, waits, index advice, verify and more), every CLI command prints JSON when piped, and finding ids are stable, so an agent can find a problem, fix the code and prove the fix worked.

```sh
claude mcp add culprit -- culprit mcp   # Claude Code; Cursor and Codex take the same command
culprit agent init                      # teach agents in this repository to use Culprit
```

You decide what agents may do, per connection: nothing, read statistics and plans (the default), or run read-only queries that you approve one by one. Approval shows the exact statement and what Culprit guarantees about it, and every tool call lands in a local audit log.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/agent-approval-dark.png">
    <img alt="An approval dialog: claude-code wants to run a SELECT on a staging connection; checked as read-only, rolled back, capped at 200 rows and 30 seconds; Deny or Run once" src="docs/images/agent-approval-light.png" width="720">
  </picture>
</p>

## Safe on production

- **Read-only by design.** Scans read statistics and catalog views only, with short lock timeouts. Estimated plans never execute anything.
- **Production is guarded.** Statements Culprit can prove only read run in a read-only transaction. Anything else on a production connection needs an explicit confirmation that says what it will do, and destructive statements need the connection's name typed.
- **Fixes are suggestions.** Every fix is shown and copied, never applied, and uses online or concurrent operations where the server supports them.
- **Secrets stay put.** Passwords live in the OS keychain, Entra ID uses your `az login`, and nothing about your servers leaves your machine. Exports redact literal values by default.
- **Agents get least privilege.** Off, statistics only, or approved read-only queries, per connection, with every call audited.

## Supported databases

| Engine | Tested on | Where the answers come from |
|---|---|---|
| **SQL Server** | SQL Server 2022, Azure SQL Database | Query Store, plan cache, DMVs, the deadlock history in extended events |
| **PostgreSQL** | PostgreSQL 15 and 18, including Amazon RDS | `pg_stat_statements`, statistics views, `EXPLAIN` (generic plans from version 12) |
| **MySQL** | MySQL 8.4 | `performance_schema`, `sys`, `EXPLAIN FORMAT=JSON` |
| **MariaDB** | MariaDB 10.11 and 11.8 | `performance_schema`, the slow query log, user statistics, `ANALYZE FORMAT=JSON`, `SHOW ANALYZE` |

When a source is switched off (Query Store, `pg_stat_statements`, `performance_schema`), Culprit shows the one statement that turns it on and what you'll get once it's on.

## Get started

Culprit builds from source today. You need [Rust](https://rustup.rs) (stable) and [Bun](https://bun.sh); on Linux, also the WebKitGTK development packages listed in the [guide](docs/guide.md#platforms). CI builds and tests every commit on macOS, Linux and Windows, and bundles the desktop app for each (an `.app`, a `.deb` and an `.msi`).

```sh
git clone https://github.com/cristipufu/culprit-sql-studio.git
cd culprit-sql-studio

cargo build --release -p culprit-cli           # the culprit CLI, in target/release
cd app && bun install && bunx tauri build      # the desktop app
```

Connect a database and get its dashboard:

```sh
culprit conn add shop --env staging --url 'postgres://app@db.example.com:5432/shop'
culprit dashboard shop       # the database at a glance
culprit scan shop            # every finding, ranked
```

Or open the desktop app, paste a connection string, and the first report appears in seconds.

**Just looking?** `cd app && bun install && bun run dev` opens the full interface in your browser with demo data: no database needed.

## Learn more

- **[Guide](docs/guide.md):** every check and command, the agent setup, measured speed, safety details and how to extend Culprit
- **[Design](docs/DESIGN.md):** how the product behaves and why
- **[Coding agents](docs/agents.md):** MCP tools, permissions, approvals and the audit log
- **[Engine parity](docs/parity.md):** every feature on SQL Server, PostgreSQL, MySQL and MariaDB, and where each answer comes from

Culprit is open source under the [MIT license](LICENSE).

<br>

<p align="center">
  <img src="docs/images/logo.svg" width="28" height="28" alt=""><br>
  <sub>Made for the people who get paged when the database is slow.</sub>
</p>
