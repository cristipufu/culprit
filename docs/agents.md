# Using Culprit from a coding agent

Culprit's CLI is designed to be driven by agents: JSON output, stable ids, no prompts, no credentials in arguments.

## Contract

- **Output.** JSON on stdout when stdout is not a terminal (or with `--json` / `--format json`). `--format md` gives a markdown table for PR comments.
- **Errors.** JSON on stderr: `{"error":{"code":"…","message":"…","hint":"…"}}`. Codes: `invalid_input`, `not_found`, `connection_failed`, `auth_failed`, `permission_denied`, `timeout`, `unsupported`, `query_failed`, `internal`.
- **Exit codes.** `0` success, `1` error, `3` findings at or above `--fail-on <severity>` (scan only).
- **Connections by name.** The person adds a connection once (`culprit conn add` or the app). Secrets live in the OS keychain, come from `az login` (Entra ID), or are read from an environment variable named with `--password-env`. Agents only ever pass the name.
- **Safe by default.** `scan`, `doctor`, `findings`, `show`, `top`, `live` and `explain` (without `--analyze`) never execute application SQL. `run` and `explain --analyze` on a production connection fail unless `--yes-execute-on-production` is passed; don't pass it without the person's explicit go-ahead.
- **Stable ids.** Finding ids are `<check>:<subject>`, e.g. `missing-index:dbo.Orders(CustomerId)` or `plan-warnings:implicit-conversion:orders.status`. The same problem keeps the same id across scans, so "is it fixed?" is "is the id gone?".

## MCP server

`culprit mcp` serves the same capabilities as MCP tools over stdio. Register it once (`claude mcp add culprit -- culprit mcp`, or `culprit agent init` in a repository) and the agent can ask "what's slow on shop-staging?" directly.

| Tool | Does |
|---|---|
| `list_connections` | Connections the agent may use: name, engine, environment, agent access, latest scan |
| `scan` | Fresh snapshot (statistics views only) and ranked findings |
| `findings` | Findings from the latest scan, filtered by category or severity |
| `finding` | One finding as a markdown brief: evidence, queries, fix, verification |
| `top_queries` | Heaviest queries by time, CPU, reads or executions |
| `query` | One query: text, statistics, plan variants, related findings |
| `query_plan` | A query's plan as an indented operator tree (stored or estimated; never executes) |
| `explain` | Estimated plan for a statement the agent provides (never executes) |
| `live` | Sessions, waits and blocking right now |
| `schema` | Tables; or one table's columns, indexes and which captured plans use each index |
| `run_query` | One read-only statement, up to 200 rows: needs `query` access and the person's approval in the app |

Results are compact JSON. Text that comes from the database (query text, object names, comments, values) is data, not instructions, and the server's instructions tell the agent so.

**Policies.** `culprit conn set <name> --agent off|stats|query`, or the Agents menu on the connection in the app. `stats` is the default. `culprit mcp --connection <name>` (repeatable) exposes only those connections.

**Approvals.** `run_query` checks the statement is a single read-only statement, then waits up to 60 seconds for the person to approve that exact text in the desktop app. A decision is bound to a SHA-256 of the connection and the SQL, so it can't approve different text. If the app isn't open, the request is denied at once with a hint to open it. Approved statements run in a transaction that is never committed (read-only on Postgres and MySQL), with a 30-second limit.

**Audit.** Every call (tool, client, connection, outcome, duration, and SQL for `explain` and `run_query`) is appended to `audit.jsonl` in the data directory. `culprit audit --json` reads it.

## Repository setup

```sh
culprit agent init                              # AGENTS.md section, .claude/skills/culprit/SKILL.md, .mcp.json entry
culprit agent init --connection shop-staging    # name the project's connections and limit the MCP server to them
culprit agent init --no-mcp                     # instructions only
```

The AGENTS.md section sits between `<!-- culprit:start -->` and `<!-- culprit:end -->` markers and is replaced in place on re-runs; everything else in the file is kept. Nothing written contains findings, host names or secrets.

## Setting up connections

```sh
culprit conn discover --match shop --match stg --json        # Azure SQL databases visible to `az login`, with suggested aliases
culprit conn discover --match shop --match stg --save        # save them (Entra ID auth, nothing secret stored; production needs --include-production)
culprit conn rename shop-stg-db shop-staging       # aliases are just names
```

## Typical loop

```sh
culprit conn list --json                                  # what can I reach?
culprit scan shop-staging --json                              # summary + ranked findings (compact)
culprit show shop-staging 'missing-index:dbo.orders(customer_id)' --json
#   → finding, the queries involved with stats and hottest plan operator, and `brief`:
#     a self-contained markdown description of the problem, evidence, fix and verification
culprit top shop-staging --by time -n 10 --json               # heaviest statements
culprit explain shop-staging --query-id 10 --json             # estimated plan of a captured query
culprit explain shop-staging --file query.sql --json          # estimated plan of a new query, before shipping it
culprit drift shop-staging shop-prod --json                   # indexes, columns, settings that differ; SQL to align
culprit plan diff before.sqlplan after.sqlplan --json         # did my index change the plan? headlines + changes
culprit plan diff shop-staging:10:7 shop-staging:10:9 --json  # two Query Store plans of query 10: why one is slower
```

`top` and `show` include `trend_mean_ms` per query: its average duration in each bucket of the window (null where it didn't run), so "when did this get slow" needs no extra call. The `regression` finding states it directly.


`plan diff` returns `headlines` (plain sentences, the slower or changed plan described against the baseline), `changes` (each with `kind`: access, operator, added, removed, cost, rows or warning, plus node ids in either plan), `cost_ratio`, and the node `matches` between the plans. Plan text from another tool works too: `culprit plan show -` reads a .sqlplan, Postgres EXPLAIN (text or JSON) or MySQL EXPLAIN JSON/tree from stdin.

After a fix ships: `culprit scan shop-staging --json` and check the finding id is gone. Findings carry `last_seen` (when their queries last ran) and `inactive: true` when those queries stopped running long before the data window ends, which usually means a deploy replaced them; treat those as history, not current problems.

## Offline and CI

```sh
culprit analyze report-or-snapshot.json --json            # re-run the checks on a saved file, no database
culprit scan staging --fail-on high                       # exit 3 when something high-impact appears
culprit findings staging --format md                      # markdown table for a PR comment
```

Reports are stored per connection under `~/.local/share/culprit/reports/<connection>/` (or `$CULPRIT_HOME/reports/`).

## JSON shapes

The types are generated from the Rust model into `app/src/gen/*.ts` and are the reference: `Report`, `Finding`, `Snapshot`, `QueryStat`, `Plan`, `PlanNode`, `ServerInfo`, `LiveState`, `ResultSet`.
