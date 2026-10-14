# How Culprit is built

Culprit turns a database's own statistics into findings a person can act on. This page is the map: the stages data
goes through, where each lives, the rules every stage follows so a finding is never a false flag, and how to add a
provider, a source, a check or a rule. `docs/DESIGN.md` is the source of truth for what the product does and says; this
page is how the code does it.

```
 database ──▶ sources ──▶ snapshot ──▶ prepare ──▶ detect ──▶ refine ──▶ rank ──▶ report ──▶ app · CLI · MCP
             (provider)   (model)      (fix the    (checks:   (rules      (impact,  (findings,  dashboard, digest,
                                        picture)    one issue  over all    then      locked      index advice, brief
                                                    each)      findings)   severity) checks)
```

Everything left of *snapshot* talks to a database; nothing right of it does. A snapshot is a plain, serializable
picture of one database at one time, so every stage after it is a pure function: the app, the CLI and the MCP server
produce the same findings, a saved scan can be analyzed again offline (`culprit analyze`), and every rule is testable
with a fixture.

## The crates

| Crate | What it owns |
|---|---|
| `culprit-core` | The model (`Snapshot`, `Plan`, `Finding`), the `Check` and `Provider` traits, plan insights, `Coverage`. No I/O. |
| `culprit-mssql`, `culprit-postgres`, `culprit-mysql` | Providers: the sources for one engine, and explain, live sessions, deadlocks. |
| `culprit-checks` | The analysis pipeline, every check, the refine rules, and the views built on a report. |
| `culprit-engine` | Connections and secrets, the provider registry, scans, saved reports and the history between scans. |
| `culprit-cli`, `culprit-mcp`, `app/` | Surfaces: the same report, worded for a terminal, a coding agent, a window. |

## 1. Sources: what a provider reads

A provider (`culprit_core::Provider`, one per engine, chosen in `culprit-engine/src/registry.rs`) collects a snapshot
from its engine's sources: Query Store and the plan cache on SQL Server, `pg_stat_statements` on PostgreSQL,
`performance_schema` digests or the slow log on MySQL and MariaDB, and on every engine the catalog, index and table
usage views, waits, settings and pressure counters.

Each source either delivers a `Capability` or doesn't. A provider keeps that account in a `culprit_core::Coverage`,
starting from what its probe found: `delivered`, `lacking` (with the reason and the fix), `lost` (the probe expected it,
the source couldn't read it, with the source's own reason), `withdraw` (expected, but it held nothing), `failed` (a
read error: a note, and the capability locked with why) and `note`. A failing source never fails the scan; the checks
that need it are listed as locked, with the fix.

Rules every source follows:

- **Safe on production.** Statistics and catalog views only, short timeouts, and every statement tagged `/* culprit */`
  so the workload never counts Culprit's own reads.
- **One database.** A snapshot describes the database it scanned. A server-, instance- or pool-wide view is filtered to
  that database where it's read (`DB_ID()`, `current_database()`), or marked shared and not graded. Another database's
  statements, deadlocks or transactions are never this one's.
- **Unknown is not zero.** A number the engine doesn't keep is `None`, never 0: zero reads of an index, zero rows of an
  `UPDATE` and zero waits are claims.
- **Say what a number covers.** A window says how many hours it really holds; cumulative counters say since when (a
  restart, a statistics reset), and a drop in a running total is a reset only when the source's own marker moved.
  PostgreSQL's counters outlive a clean restart and are discarded by a crash recovery that keeps the start time, so
  their age is the one the scan measured (`stats_reset`, else when statistics were last discarded), never the time
  since the server started.
- **Planning never lends this login's privileges.** PostgreSQL's planner runs the functions and operators a statement
  reaches (constants folded, a column's common values compared), with the privileges of whoever plans it, and neither
  `search_path` nor `SET ROLE` keeps someone else's code out. `culprit-postgres/src/trust.rs` makes every plan in a
  read-only transaction that is rolled back, and a login that can do more than read statistics doesn't plan while
  functions or operators other roles wrote are in reach.
- **The plan that runs now.** A plan describes the database when it was captured. Query Store keeps a plan's first XML,
  so a plan older than its table is marked in the prepare stage.

## 2. The snapshot

`culprit_core::Snapshot` is the normalized model: the workload (statements, their runs and time, plans, trends), tables,
indexes, columns, foreign keys, statistics, waits, settings, pressure signals, and what the scan could and couldn't read
(`server.capabilities`, `server.missing`, `notes`). Engine-specific data rides in optional fields, so a check that
applies to every engine is written once. Types the UI shows derive `ts_rs::TS`; `cargo test -p culprit-core -p
culprit-engine` regenerates `app/src/gen`.

## 3. The pipeline: from a snapshot to a report

`culprit-checks/src/pipeline.rs` runs four stages, in order, each a list you can read top to bottom:

1. **Prepare**: make the snapshot say what the database says now, before anything reads it. Today: plans captured
   before a column changed type (`culprit_core::insights::mark_snapshot`).
2. **Detect**: every check that applies to the engine and can run with what the login sees (`culprit_checks::all()`).
   A check that needs a missing capability is reported as locked, with the reason, instead of running on missing data.
3. **Refine**: rules over all the findings (`culprit-checks/src/refine.rs`): two findings about one cause become one,
   and a finding that rests on statements whose cost is behind them (they stopped running, or ran once) says so and
   ranks low. A rule here applies to every check on every engine.
4. **Rank and summarize**: active findings first, by impact; then the headline (`summary.rs`).

`culprit_checks::analyze(snapshot)` runs the built-in pipeline. `Pipeline::builtin().with_check(..).with_refine(..)
.with_prepare(..)` adds an embedder's own; they run, rank and summarize exactly like the built-in ones, and a plug-in
check can't take over a built-in id.

### Checks

A check looks for one kind of problem: `<name>_info()` (id, title, category, the engines it applies to, the
capabilities it needs) and `<name>(&Snapshot) -> Vec<Finding>`, registered in `culprit-checks/src/lib.rs::all()`. A
finding has a stable id (`<check>:<subject>`), a title and a plain-language summary, evidence (metrics, the queries
behind it), a fix that is safe to run, a way to verify it, a severity, a confidence and an impact used for ranking.

Plan insights (`culprit-core/src/insights.rs`) are rules over one plan, shown in the plan viewer; the `plan-insights`
check promotes the ones that matter to findings.

### The evidence rules: what keeps a finding from being a false flag

Every check and insight follows these. Each has one implementation; use it rather than restating it.

| Rule | Where |
|---|---|
| A rate compared with a threshold needs at least an hour of data; a sentence that says "a day" needs 20 hours, otherwise it says what was counted and over how long. | `RATE_MIN_HOURS`, `PER_DAY_MIN_HOURS`; `support::rate_per_day`, `counter_rate_per_day`, `how_many`, `runs_phrase` |
| No reads, no scans, nothing happened: a claim of absence needs a week of counters (a weekly job has had a chance to run). | `indexes::is_unused`, `watched_long_enough` |
| A statement that ran once, or stopped running, is history: its findings say so, rank low and get no index of their own. | refine rules `statements that ran once` / `statements that stopped running`; `query_is_history` |
| A claim about a cause is the engine's own verdict, never inferred from a plan's shape ("stops a seek" needs SQL Server's Seek Plan warning). | `insights::implicit_conversion` |
| A type mismatch is a column the engine converts implicitly to match a value of another type, in the plan that runs now. | `insights::converts_column`, `current_conversions`, `mark_changed_columns` |
| An index is advice only for a filter that keeps a small share of the rows read; past that, the scan is the right plan and the advice is to read less. | `INDEX_KEEPS_AT_MOST`, `PlanNode::kept_share` |
| A setting is changed where the server runs: SQL on a server you manage, the platform's control on a managed one, and never to a lower value than it has. | `support::setting_change` |
| Severity needs an amount, not only a share: a large share of a quiet database's time is still a small number (Medium needs a minute a day of database time, High ten). Time spent since a recent change is rated over its own hours; a window of unknown length is never High. Counts are rated by what they cost (a chatty statement's round trips at about a millisecond each). | `support::severity_for_time`, `severity_for_workload_share`, `severity_for_recent_time` |
| One cause, one finding. | refine rules |
| Fix SQL must never damage what exists: rebuilding an index keeps its uniqueness, filter, key order and included columns, or there is no SQL. | per fix |

### Views built on a report

The dashboard (`dashboard.rs`), the digest for scheduled jobs (`digest.rs`), the index advisor (`advisor.rs`), the
agent brief (`brief.rs`) and drift between two databases (`drift.rs`) read a report and add no findings of their own;
they follow the same evidence rules.

## Extending Culprit

- **A check**: write `<name>_info()` and `<name>(&Snapshot)` in the module for what it looks at, register it in
  `all()`, give each finding a stable id, follow the evidence rules, and test it on a fixture (`fixtures/*-demo.json`)
  for the case it should flag and the cases it must not.
- **A refine rule**: a function `fn(&Snapshot, &mut Vec<Finding>)` in `refine.rs`, named for the duplicate or false flag
  it removes, listed in `rules()`.
- **A prepare step**: a function `fn(&mut Snapshot)` listed in `pipeline.rs::preparation()`. It corrects only what the
  data contradicts (a plan older than its table), never to hide a problem.
- **A source**: read it in the provider, record it in the provider's `Coverage`, fill the snapshot's fields (`None` for
  what the engine doesn't keep), and follow the source rules above.
- **A provider**: implement `culprit_core::Provider` in a crate, add it to `culprit-engine/src/registry.rs` behind a
  cargo feature; an embedding application can also supply one at runtime with `Workspace::with_provider`.
- **At runtime, without forking**: `Workspace::open()?.with_check(Arc::new(MyCheck))`, or a `Pipeline` with extra
  checks, prepare steps and rules (`crates/culprit-checks/tests/plugin.rs`).

## Testing

- Every check and rule has tests on the demo snapshots (`fixtures/*-demo.json`, `fixtures/*.report.json`), including
  the cases it must not flag.
- `dev/postgres` and `dev/mssql` are Docker databases with planted problems; regenerate a UI fixture with
  `culprit analyze <snapshot.json> --connection <name> --json > fixtures/<name>.report.json`.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --all`.
