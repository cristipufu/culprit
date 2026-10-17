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
| `culprit-mssql`, `culprit-postgres`, `culprit-mysql`, `culprit-sqlite` | Providers: the sources for one engine, and explain, live sessions, deadlocks. SQLite reads a file, read-only: schema, statistics, settings and plans, no sessions or waits. |
| `culprit-querylog` | Logs an application's framework already writes (Rails, Django, EF Core, Phoenix/Ecto, JSON lines), read into a `Workload` for an engine that keeps no statement history: one module per format behind a `Format` trait, statements grouped by `culprit_core::fingerprint`. The log's own timestamps are its period (`Workload.log`), never "the last N hours". |
| `culprit-checks` | The analysis pipeline, every check, the refine rules, and the views built on a report. |
| `culprit-engine` | Connections and secrets, the provider registry, scans, saved reports and the history between scans. |
| `culprit-cli`, `culprit-mcp`, `app/` | Surfaces: the same report, worded for a terminal, a coding agent, a window. |

## 1. Sources: what a provider reads

A provider (`culprit_core::Provider`, one per engine, chosen in `culprit-engine/src/registry.rs`) collects a snapshot
from its engine's sources: Query Store and the plan cache on SQL Server, `pg_stat_statements` on PostgreSQL,
`performance_schema` digests or the slow log on MySQL and MariaDB, and on every engine the catalog, index and table
usage views, waits, settings and pressure counters.

Each provider declares its sources in one list, `<provider>/src/sources.rs`: a `culprit_core::Source` per view or log
it reads, with its name (what its notes carry), what it reads, and the capabilities it delivers to the checks.
`culprit sources` prints every engine's list. A scan reads each one through `Coverage::read`, the one implementation of
"a failing source never fails the scan": a note with the error, and the capabilities it delivers lost with the source's
own reason (a permission error says the login lacks it, with the grant as the fix). A source that delivers no
capability adds detail some checks use; without it they say less, never something wrong.

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
  read-only transaction that is rolled back. A login that can do more than read statistics plans only its own copy of
  a statement (during a scan, and on demand through `Provider::explain_recorded`), and nothing while functions or
  operators other roles wrote are in reach. Culprit's own statements in a scan resolve names in the catalog only
  (`search_path = pg_catalog, pg_temp`); a plan uses the database's own search_path.
- **The plan that runs now.** A plan describes the database when it was captured. Query Store keeps a plan's first XML,
  so a plan older than its table is marked in the prepare stage.
- **Say what kind of source it is.** A provider sets `Workload::kind` (`WorkloadKind`: Query Store, the plan cache,
  pg_stat_statements, performance_schema, the slow log, an application's log) beside the source's name for sentences.
  The rules that depend on it (a slow log keeps only slow runs, performance_schema's histograms count from the start)
  ask `Workload::kind()`, never the name; scans saved before it existed are read back in that one place. The same for a
  product that rides on another's provider: `ServerInfo::is_mariadb()`, never a name compared.
- **Date the counters.** Usage counters count from the latest moment they may have started: the server's start, a
  statistics reset, a database restored since (SQL Server's `create_date`), one that reopens with AUTO_CLOSE, MariaDB's
  userstat turned on. The collector dates what it can see (`IndexStat::counters_since`) and the scans add what they saw
  (`culprit-engine/src/history.rs::date_new_indexes`); absence waits a week from then.

### Platforms: where a database runs

A managed service changes how Culprit reads and advises (no superuser, settings in a parameter group, the cloud's own
metrics). It is one type, `culprit_core::Platform`, detected once by the provider that can see it and recorded on
`ServerInfo::platform` (older reports carry only the `cloud` label, which `ServerInfo::platform()` reads back). Checks and
the engine ask it (`settings_control`, `preload_library`, `metrics`, `is_azure_sql_database`); nothing matches on
platform names. A new platform is a variant in `crates/culprit-core/src/platform.rs` with its answers there, and its
detection in the provider.

### Engines: one file each for what Culprit knows about an engine

Everything Culprit knows about an engine apart from its provider lives in one file, `crates/culprit-core/src/engines/
<engine>.rs`, behind the `EngineSpec` trait (`Engine::spec()`):

- its name (`label`) and what it can answer (`abilities`: sessions, wait samples, deadlocks, measured runs, plan
  estimates, writes, periods to compare, last actual plans, live per-step progress, a sign-in);
- how it spells the SQL Culprit suggests (`Dialect`: quoted names, an index built or dropped without blocking writes,
  statistics refreshed, a session ended);
- how it says what every check needs to say about it: applying a setting, refreshing a table's statistics, why a wait
  isn't put on statements, where the index advisor's candidates come from;
- what Live may do to a session, and why it refuses what it can't do (`refusal`: the message and the hint every surface
  returns, providers included).

Checks, insights and providers ask the spec instead of matching the engine. What a single check says about one engine
(how to fix a spill on PostgreSQL, its thresholds) stays in the check. The same facts reach the app as
`app/src/gen/engines.json` (written by `cargo test -p culprit-core`), so views offer only what an engine can answer and
the browser demo answers as the engine does without a copy of its own. A new engine is one file here, one arm in
`Engine::spec`, and its provider crate.

## 2. The snapshot

`culprit_core::Snapshot` is the normalized model: the workload (statements, their runs and time, plans, trends), tables,
indexes, columns, foreign keys, statistics, waits, settings, pressure signals, and what the scan could and couldn't read
(`server.capabilities`, `server.missing`, `notes`). Engine-specific data rides in optional fields, so a check that
applies to every engine is written once. Types the UI shows derive `ts_rs::TS`; `cargo test -p culprit-core -p
culprit-engine` regenerates `app/src/gen`.

A saved scan is read as this version would read it. Whenever one is loaded (`culprit analyze`, the latest report the
app, CLI, MCP and exports show, the index advisor, the digest), `Workspace::reread` parses each plan again from the text
it was captured as (`culprit_engine::plans::reparse_saved`, through each provider's `reparse`: `plan_json::reparse` on
PostgreSQL and MySQL, `plan_xml::reparse` on SQL Server) and runs the pipeline again, so a parser or check fix reaches
the scans saved before it. What the scan added that the text doesn't hold stays as saved (the statement it planned, the
server's version, where a sample came from and its LIMIT, table sizes), and so do SQLite's plans, whose text keeps the
steps but not their tree.

## 3. The pipeline: from a snapshot to a report

`culprit-checks/src/pipeline.rs` runs four stages, in order, each a list you can read top to bottom:

1. **Prepare**: make the snapshot say what the database says now, before anything reads it
   (`pipeline.rs::preparation()`): plans captured before a column changed type, MySQL's possible keys named with their
   columns, plan operators marked with their index's kind from the catalog (a BRIN bitmap counts pages, not rows), the
   health counters' grades held to the evidence rules (`pressure::grade_on_evidence`), PostgreSQL's WAL write and sync
   time kept out of statement-time waits in scans saved before the collector did so (`statement_time_without_wal`), and
   MariaDB's lock time read as unknown where the slow log to a table counts a statement's whole run as it
   (`lock_time_that_counts_the_run`, through `signals::lock_time_counts_the_run`).
2. **Detect**: every check that applies to the engine and can run with what the login sees (`culprit_checks::all()`).
   A check that needs a missing capability is reported as locked, with the reason, instead of running on missing data.
3. **Refine**: rules over all the findings (`culprit-checks/src/refine.rs`): two findings about one cause become one,
   and a finding that rests on statements whose cost is behind them (they stopped running, or ran once) says so and
   ranks low. A rule here applies to every check on every engine.
4. **Rank and summarize**: active findings first, by severity, then by impact; then the headline (`summary.rs`).

`culprit_checks::analyze(snapshot)` runs the built-in pipeline. `Pipeline::builtin().with_check(..).with_refine(..)
.with_prepare(..)` adds an embedder's own; they run, rank and summarize exactly like the built-in ones, and a plug-in
check can't take over a built-in id.

### Checks

A check looks for one kind of problem: `<name>_info()` (id, title, category, the engines it applies to, the
capabilities it needs) and `<name>(&Snapshot) -> Vec<Finding>`. `check!(NameCheck: name_info, name)` makes it a type
implementing `culprit_core::Check`, the same trait a plugged-in check implements, and the area lists it in its `CHECKS`
(`all()` runs every area's, in the order of `checks::AREAS`; `tests/registry.rs` fails when a check is written but not
listed). Every
check has its own file, in a directory per area under `culprit-checks/src/checks/` (`queries/`, `indexes/`,
`signals/`, `pressure/`, `maintenance/`, `tuning/`…; `checks/mod.rs` maps them); what a few checks of one area share
sits in that area's `mod.rs`, and what every check shares (the evidence rules) in `support.rs`. A
finding has a stable id (`<check>:<subject>`), a title and a plain-language summary, evidence (metrics, the queries
behind it), a fix that is safe to run, a way to verify it, a severity, a confidence and an impact used for ranking.

Plan insights (`culprit-core/src/insights.rs`) are rules over one plan, shown in the plan viewer; the `plan-insights`
check promotes the ones that matter to findings.

### The evidence rules: what keeps a finding from being a false flag

Every check and insight follows these. Each has one implementation; use it rather than restating it.

| Rule | Where |
|---|---|
| A rate compared with a threshold needs at least an hour of data; a sentence that says "a day" needs 20 hours, otherwise it says what was counted and over how long. Runs said as they came: a burst is when it ran ("750 runs on Oct 11, 15:51–15:55 UTC"), fewer runs than days a count ("1 run in the last 6 days"). | `RATE_MIN_HOURS`, `PER_DAY_MIN_HOURS`; `support::rate_per_day`, `counter_rate_per_day`, `how_many`, `runs_phrase`; `Workload::cadence`, `how_often_of` |
| On a source of running totals (pg_stat_statements, performance_schema, the plan cache), a statement's rate and share are what Culprit's own scans measured, over 20 hours or more of them, with the average since the totals began beside it where the two differ twofold; a recent entry is judged by its share over the scans. A change shows between two scans (What changed), never in the totals, and a rewritten statement is a new entry. | `Workload::over_scans`, `how_often_of`, `cadence`; `queries::recent_entry`, `verify_between_scans` |
| No reads, no scans, nothing happened: a claim of absence needs a week of counters (a weekly job has had a chance to run). | `indexes::is_unused`, `watched_long_enough` |
| A statement that ran once, or stopped running, is history: its findings say so, rank low and get no index of their own. | refine rules `statements that ran once` / `statements that stopped running`; `query_is_history` |
| A claim about a cause is the engine's own verdict, never inferred from a plan's shape ("stops a seek" needs SQL Server's Seek Plan warning). | `insights::implicit_conversion` |
| A type mismatch is a column the engine converts implicitly to match a value of another type, in the plan that runs now. | `insights::converts_column`, `current_conversions`, `mark_changed_columns` |
| An index is advice only for a filter that keeps a small share of the rows read; past that, the scan is the right plan and the advice is to read less. | `INDEX_KEEPS_AT_MOST`, `PlanNode::kept_share` |
| A setting is changed where the server runs: SQL on a server you manage, the platform's control on a managed one, and never to a lower value than it has. | `support::setting_change` |
| Severity needs an amount, not only a share: a large share of a quiet database's time is still a small number (Medium needs a minute a day of database time, High ten). Time spent since a recent change is rated over its own hours; a window of unknown length is never High. Counts are rated by what they cost (a chatty statement's round trips at about a millisecond each). | `support::severity_for_time`, `severity_for_workload_share`, `severity_for_recent_time` |
| A number counted for the whole server (instance waits and compiles, server-wide connections, InnoDB's lock time, another database's consumers) is said as the server's and isn't graded on this database, unless this database's own statements show it. | per check; `pressure::grade_on_evidence` for the health squares |
| An optimizer's default guess isn't an estimate (MySQL's fixed `filtered`, PostgreSQL's default selectivity), and what the runs measured outweighs what the plan estimated. | `insights::mysql_guessed_filtered`, `plan::GUESSED_SELECTIVITY`, `PlanNode::kept_share`, `filtered_scan::measured_keeps_few` |
| Index keys come only from conditions an index can seek on: a plain column compared with a value. An OR across columns, `<>`, two columns compared, arithmetic or a cast on the column, a hash join's bitmap probe give none. | `insights::seekable_columns`, `support::predicate_columns` |
| Statements a driver or a pool sends (BEGIN, COMMIT, SET, DISCARD ALL, `SELECT 1`) aren't the application's: no N+1, no "find it in the code". | `sql::is_session_statement` |
| A plan insight states a share of the plan's work only where it measured one; a rule's own weight is not shown as a share and raises nothing. | `PlanInsight::fixed_weight` |
| A plan's numbers say what they are: rows read are what the engine reports, or a table's size only for a scan that reads it in full; estimates are compared with actual rows for the runs that happened; a step's share is of what the plan's costs add up to, and none is stated beside a step the plan puts no cost on or above a guessed estimate. | `PlanNode::rows_read_total`, `est_rows_for_runs`, `PlanNode::misestimates`, `Plan::cost_share`, `Plan::step_share` |
| A plan's step is where a statement's work is only in a plan that does real work (a measured run of 10 ms, or an estimate of what reading about 10K rows costs), and only for runs measured cheap (under 10 ms, a few pages per row, rows near the plan's estimate, no table read whole) is a statement's time "how often it runs". A finding states its step's share of the plan only there, on every surface (brief, exports, app). Advice for a step (a covering index, a sorted index, an index for a scan) follows the plan's own rule for that step. | `insights::does_real_work`, `working_step`, `runs_cheaply`, `has_insight`; `finding_step_share` (`Plan::step_share` in a plan that does real work), `no_share_reason`; `filtered_scan::scan_groups` |
| No index, statistics or rewrite advice on the engine's own objects (`sys.*`, `pg_catalog`, `information_schema`, `performance_schema`, `sqlite_*`): nothing an application changes. | `insights::on_system_objects` |
| A statement's share is graded as the source allows: where entries count from different moments (the plan cache counts each plan from its compile), a statement claims at most its share of the time an hour, so a plan cached for a month doesn't outweigh one compiled an hour ago by its age. | `Workload::share_of`, `share_weight`, `hourly_time_ms`; `support::query_impact` |
| One cause, one finding. | refine rules |
| Fix SQL must never damage what exists: rebuilding an index keeps its uniqueness, filter, key order and included columns, or there is no SQL. | per fix |

### Views built on a report

`culprit-checks/src/views/`: the dashboard (`dashboard/`, one file per tile), the digest for scheduled jobs
(`digest.rs`), the index advisor (`advisor.rs`), the agent brief (`brief.rs`), drift between two databases
(`drift.rs`) and the waits explained (`waits.rs`) read a report and add no findings of their own; they follow the same
evidence rules.

## Extending Culprit

- **A check**: write `<name>_info()` and `<name>(&Snapshot)` in a file of its own in the area's directory under
  `checks/` (declared in the area's `mod.rs`), declare its type with `check!`, add it to the area's `CHECKS` list, give each finding a stable id, follow the evidence rules, and test it on a fixture (`fixtures/*-demo.json`)
  for the case it should flag and the cases it must not.
- **A refine rule**: a function `fn(&Snapshot, &mut Vec<Finding>)` in `refine.rs`, named for the duplicate or false flag
  it removes, listed in `rules()`.
- **A prepare step**: a function `fn(&mut Snapshot)` listed in `pipeline.rs::preparation()`. It corrects only what the
  data contradicts (a plan older than its table), never to hide a problem.
- **A source**: declare it in the provider's `src/sources.rs` (and its `ALL` list), read it through `Coverage::read`
  (SQL Server: `sources::read`, for its driver's errors), fill the snapshot's fields (`None` for what the engine
  doesn't keep), and follow the source rules above.
- **A provider**: implement `culprit_core::Provider` in a crate, add it to `culprit-engine/src/registry.rs` behind a
  cargo feature; an embedding application can also supply one at runtime with `Workspace::with_provider`.
- **At runtime, without forking**: `Workspace::open()?.with_check(Arc::new(MyCheck))`, or a `Pipeline` with extra
  checks, prepare steps and rules (`crates/culprit-checks/tests/plugin.rs`).

## Why this shape

Tools in this category take one of a few shapes. What decides between them here is what Culprit promises: it is run
against production, every finding must be true (a false flag costs the reader's trust), it covers several engines, and
anyone should be able to add a check.

| Shape | What it buys | Why Culprit doesn't take it |
|---|---|---|
| Each check is a script run on the server (sp_Blitz, a folder of SQL per engine) | A check is one query; easy to start | Every check touches production and needs its own permissions; nothing can be analyzed offline or tested on a fixture; each engine repeats each check; and a rule such as "absence needs a week of counters" is restated in every script. |
| Each check queries the database itself | No shared model to maintain | The same views are read many times, every check needs a connection, and a failing read fails the check instead of locking it with why. One read per source into a snapshot gives the checks a pure, testable input. |
| One set of checks per engine | No normalized model | The same problem (a missing index, a spill) is written four times and drifts. A normalized snapshot lets a check be written once; what differs per engine lives in its `EngineSpec` and its provider. |
| Rules as configuration (thresholds in YAML, a rule DSL) | Thresholds change without code | The rules that keep findings true (rates only over an hour of data, the engine's verdict over a plan's shape, shared counters not graded) are logic, not thresholds; a DSL that can say them becomes a programming language without a compiler or tests. |
| Checks loaded at run time (dynamic libraries, WASM) | Extend an installed binary | Rust has no stable plugin ABI, and code loaded into a tool that reads production is a risk. An embedding application plugs in checks, sources and rules through the same traits at compile time (`Workspace::with_check`, `with_provider`, `Pipeline::with_*`); a WASM host could sit behind `Check` later without changing the shape. |
| Checks that register themselves (link-time lists, macros) | No list to edit | Which checks run, and in what order, is hidden. Explicit per-area lists are one line each, and `tests/registry.rs` fails when a check is written but not listed, so the safety is the same and nothing is magic. |
| A collector agent streaming metrics (exporter + time series) | Continuous history | It needs an agent on every server. The engines already keep the history that matters (Query Store, pg_stat_statements, performance_schema); Culprit reads it on demand and compares its own scans for the rest. |

Measured on this code in October 2026:

- **Adding a check** (a real one, "large tables without a primary key", on a throwaway branch): one new file of 33
  lines, and three lines in its area's `mod.rs` (`mod`, `pub use`, its `CHECKS` entry). It ran at once (`culprit
  checks` listed 53); forgetting the `CHECKS` line made `tests/registry.rs` fail.
- **Self-registering checks** would save the one `CHECKS` line, nothing more: Rust needs the `mod` declaration
  whatever registers the check.
- **Rules as configuration**: at most 9 of the 52 checks compare fields with thresholds and nothing else. The other 43
  use the evidence rules: rates over a real window (24 checks), severity from an amount of time (18), statements that
  are history (19), relations across objects (15), shares weighed by the source (18), reading plans (6).
- **Rules as configuration, built and run**: `xid-age`, `heap-table` and `unused-index` written as a 24-line TOML rule
  file, evaluated over the same 18 snapshots (the six demos, and variants that make each check speak) as the Rust
  checks. Their findings and severities agree on all 54 runs, but only once the format had an expression language
  (269 lines) and 11 builtins (133 lines), five of which restate the evidence rules and relations in code; the wording
  (titles, summaries, fixes, which vary with the evidence) isn't covered and would need a template language with
  conditions. A misspelled field (`item.is_hep`) made the rule silently find nothing where the check finds 4; in Rust
  it doesn't compile.
- **A script per check** would restate the evidence rules at each of the 183 places checks call them today, so one
  correction (as the false-flag review made dozens of) becomes up to 183 edits instead of one.
- **Reading sources**: the four providers had 39 places handling a failed read by hand, with differing wording and
  rules; declared sources and `Coverage::read` made it one rule, and each demo scan reads the same capabilities and
  notes as before.
- **A fifth engine**: a provider crate, one `EngineSpec` file and one line in `Engine::spec`, then the 52 matches that
  name every engine (21 in core, 19 in checks, 12 in the engine crate), which the compiler lists because they are
  exhaustive; most of those in checks are a check's own wording for each engine, which stays with the check.

What the chosen shape costs: the snapshot model grows with each engine's own fields (they stay optional, `None` when
an engine doesn't keep them), checks are compiled in rather than dropped in, and a new engine is real work (a provider
crate and an `EngineSpec`). In exchange, every finding comes from one read of each source, passes the same evidence
rules, and is reproducible offline from a saved scan.

## Testing

- Every check and rule has tests on the demo snapshots (`fixtures/*-demo.json`, `fixtures/*.report.json`), including
  the cases it must not flag.
- What a person reads in each demo fixture (every finding with its evidence, fix, verify steps, caveats and brief, and
  every plan's insights) is one plain text (`culprit_engine::reading`) compared with `fixtures/golden/<name>.txt` by
  `crates/culprit-engine/tests/golden.rs`, so a fix that changes other findings' wording shows up as a diff; accept
  an intended change with `CULPRIT_UPDATE_GOLDEN=1 cargo test -p culprit-engine --test all golden::`.
- `dev/postgres` and `dev/mssql` are Docker databases with planted problems; regenerate a UI fixture with
  `culprit analyze <snapshot.json> --connection <name> --json > fixtures/<name>.report.json`.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --all`.
