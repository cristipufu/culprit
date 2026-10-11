# Using Culprit from a coding agent

Culprit's CLI is designed to be driven by agents: JSON output, stable ids, no prompts, no credentials in arguments.

## Contract

- **Output.** JSON on stdout when stdout is not a terminal (or with `--json` / `--format json`). `--format md` gives a markdown table for PR comments.
- **Errors.** JSON on stderr: `{"error":{"code":"…","message":"…","hint":"…"}}`. Codes: `invalid_input`, `not_found`, `connection_failed`, `auth_failed`, `permission_denied`, `timeout`, `unsupported`, `query_failed`, `internal`.
- **Exit codes.** `0` success, `1` error, `3` findings at or above `--fail-on <severity>` (scan only).
- **Connections by name.** The person adds a connection once (`culprit conn add` or the app). Secrets live in the OS keychain, come from `az login` (Entra ID), or are read from an environment variable named with `--password-env`. Agents only ever pass the name.
- **Safe by default.** `scan`, `doctor`, `findings`, `show`, `top`, `live` and `explain` (without `--analyze`) never execute application SQL. `run` and `explain --analyze` on a production connection fail unless `--yes-execute-on-production` is passed; don't pass it without the person's explicit go-ahead.
- **Stable ids.** Finding ids are `<check>:<subject>`, e.g. `missing-index:dbo.Orders(CustomerId)` or `plan-warnings:implicit-conversion:orders.status`. The same problem keeps the same id across scans, so "is it fixed?" is "is the id gone?".

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
```

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
