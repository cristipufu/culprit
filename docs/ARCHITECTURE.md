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
| `culprit-mssql`, `culprit-postgres`, `culprit-yugabyte`, `culprit-mysql`, `culprit-sqlite` | Providers: the sources for one engine, and explain, live sessions, deadlocks where supported. YugabyteDB owns DocDB measurements and shares PostgreSQL wire transport, not its heap collectors. SQLite reads a file, read-only: schema, statistics, settings and plans, no sessions or waits. |
| `culprit-querylog` | Logs an application's framework already writes (Rails, Django, EF Core, Phoenix/Ecto, JSON lines), read into a `Workload` for an engine that keeps no statement history: one module per format behind a `Format` trait, statements grouped by `culprit_core::fingerprint`. The log's own timestamps are its period (`Workload.log`), never "the last N hours". |
| `culprit-checks` | The analysis pipeline, every check, the refine rules, and the views built on a report. |
| `culprit-engine` | Connections and secrets, the provider registry, scans, saved reports and the history between scans. |
| `culprit-cli`, `culprit-mcp`, `app/` | Surfaces: the same report, worded for a terminal, a coding agent, a window. |

### Workspace services

`Workspace` keeps the public API and owns its configured stores, checks, authentication and provider factories.
It constructs borrowed services from those dependencies for each operation, so replacing a secret store or provider
factory applies to subsequent calls without maintaining a second copy of configuration:

- `connection.rs::Connections` owns connection metadata, imports/discovery, credential and token resolution, certificates
  and provider creation. It receives the stores, authentication handler and provider factories explicitly.
- `scan.rs::Scan` owns one operation's clock, history enrichment, analysis handoff and persistence. Its clock starts
  before provider authentication; the façade supplies the primary snapshot and handles rejected credentials. The
  remaining operation receives a provider, report directory and checks, and consumes its scan context once.
- `reporting.rs::Reports` owns saved-file replay, exact report/window selection, history, digest and advisor views.
  It receives paths and checks, with no provider or secret store, so offline reads cannot open a database.
- `execution.rs::Execution` combines connection resolution and report context for plans, live reads, queries and
  session actions. Read-only, server-parser, production and confirmation guards stay at this boundary.
- `event_context.rs::Events` combines the same dependencies for period comparisons, marks, historical events and
  cloud context. Each operation uses that workspace's report directory and the requested period.

Services do not hold a `Workspace` or call the façade to perform their work. Existing modules continue to own storage,
provider registration, cloud transport and pure analysis; services compose them without duplicating their rules.

Interactive database work uses the shared policy in `culprit-core/src/limits.rs`: 30 seconds for queries and measured
plans, 10 seconds for observations, and at most 10,000 retained result rows or 8 MiB of cell data. `execution.rs` bounds
the provider future, including database connection setup and server validation, with one deadline per operation;
human credential sign-in happens before that deadline. Wait samples have their requested duration plus bounded
overhead. Dropping a caller drops its driver future and session. Native workers keep an `OperationGuard` in the async
caller and observe its handle before starting work and in SQLite's VM progress handler, including time spent queued.
Providers use `ResultBudget` before allocating display values and drain network results through completion even after
retention stops, so a late failure cannot become a successful write. Runtime providers follow the same cancellation
and retention contract; the workspace also clamps the row request before handing it to them.

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

Each read also records a typed `SourceOutcome` in `Snapshot.sources`: `Collected`, `Empty`, `Partial`,
`Unavailable`, or `NotApplicable`, with a reason where needed. `Coverage::read_rows` distinguishes a successful empty
catalog from a failed read. Composite readers call `completed` after decoding, `partial` when some evidence could not
be read, and `not_applicable` only after establishing that a source does not apply. Concurrent parts merge
conservatively: one successful part cannot erase a failure. Detail collectors use `Coverage::append_to`; top-level
collectors retain `outcomes()` before consuming the original `finish()` tuple. Informational `note`s do not change
outcomes. Scans saved before outcomes existed retain their conservative note interpretation in one legacy adapter,
`Snapshot::source_problem`.

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

### Provider collection boundaries

PostgreSQL and MySQL/MariaDB keep collection responsibilities in separate modules. `probe.rs` establishes server
identity, capabilities and source availability. `catalog.rs` reads tables, indexes, columns, foreign keys, usage and
settings. `workload.rs` reads statement counters, families or digests, slow-log fallback and safe plan capture, returning
an explicit `CollectedWorkload` to its caller. `collect.rs` owns concurrent source orchestration, coverage accounting
and snapshot assembly. Individual source errors still become notes and unavailable capabilities in that one place.

PostgreSQL's native parser owns generic-plan and normalized-statement annotations in `plan_json.rs`, so reparsing a
saved plan does not depend on collection orchestration. Shared identifier quoting stays with the SQL protocol helpers
in `rows.rs`. The public provider traits and PostgreSQL's SQL helper exports retain their original interfaces.

YugabyteDB has its own probe, catalogs, workload, native DIST plan reader and live sources. The shared `wire` boundary
in `culprit-postgres` supplies authenticated/TLS sessions, planning ownership checks and bounded SQL execution.
`EngineSpec::sql_syntax` shares YSQL's PostgreSQL grammar without inheriting heap, buffer-cache or vacuum measurements.
Native index hash groups remain separate from ordered keys. Distributed workload comparisons bind the contributing
node set and each node's reset epoch through `CounterSet`; partial contributors or unknown epochs cannot establish a
continuous counter history. Local index zeros cannot establish cluster-wide absence.

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
app, CLI, MCP and exports show, the index advisor, the digest), the saved-report service (`reporting.rs`) parses each plan again from the text
it was captured as (`culprit_engine::plans::reparse_saved`, through each provider's `reparse`: `plan_json::reparse` on
PostgreSQL and MySQL, `plan_xml::reparse` on SQL Server) and runs the pipeline again, so a parser or check fix reaches
the scans saved before it. What the scan added that the text doesn't hold stays as saved (the statement it planned, the
server's version, where a sample came from and its LIMIT, table sizes), and so do SQLite's plans, whose text keeps the
steps but not their tree. A PostgreSQL plan saved without a table's size takes it from the scan's own catalog
(`tables::refill`), and a timeline of Culprit's scans is built again from the counters files the scans before it kept
(`reports::rebuild_timeline`), so a step a saved scan misread reads as the counters say.

### Retained evidence and provenance

`Report.snapshot` is the prepared view. `Report.evidence.input` retains the snapshot before pure preparation;
Workspace scans additionally retain the provider's original picture in `evidence.collected`, before history adds
baselines, dates counters or sets aside replica usage. Database-recorded change events remain separate. Replaying a
report starts from retained input, reparses its native plans and reruns today's preparation and checks. The original
collection is preserved, and a reread never rewrites the saved file. Replays do not nest reports or grow a version chain.
A report without retained evidence uses its prepared picture with the explicit `PreparedReport` origin: it cannot
recover data an older preparation discarded, and it never claims to hold the original collection.

`Report.provenance` records the analyzer and collector component versions, optional build revisions, actual pipeline
registration order, and extension versions supplied by their `version()` methods. Unknown extension or external-provider
versions remain unknown. Plan parsing records the native parser component and whether each saved query plan was
reparsed, lacked native text, or was retained. Baseline references identify the actual historical scan ids, dates and
database identities used for workload trends, query activity, inferred windows, index ages, growth and counters. Only
workload comparisons carry workload measurement context; index and operational counters do not borrow that clock.
Older baselines with only dates keep only dates, and never acquire today's endpoint or an id guessed from a rounded
timestamp. Index-age history uses only this database's past scans; older event sidecars recover their source identity
from their own saved reports within a bounded backfill.

Saved reports use a versioned private JSON encoding: the prepared snapshot appears once, with lossless flat-path
differences for retained input and collection. Loading restores the full report; older full-report JSON remains
readable. Immutable native plan text is shared in memory across these snapshots. Report lists read small summary
sidecars, or stream only the header of an older report, without deserializing its plans and retained evidence.

Consumers request only what they need. `latest_snapshot` and scan-to-scan comparisons read the prepared snapshot
directly, skipping derived views and retained evidence. Analysis readers reconstruct retained input and collection
without decoding a typed prepared snapshot or obsolete derived views. They share the same origin/provenance
constructors as in-memory replay, validate retained differences, and never rewrite a saved artifact.

For a repeatable offline profile, `profile_reports` creates a synthetic workload from the checked-in SQL Server
fixture. Run each measured mode separately so preparation does not inflate its peak memory:

```sh
culprit_profile_dir=$(mktemp -d "${TMPDIR:-/tmp}/culprit-profile.XXXXXX")
cargo run --locked -p culprit-engine --example profile_reports -- prepare "$culprit_profile_dir" 1000
target/debug/examples/profile_reports load "$culprit_profile_dir" 3
target/debug/examples/profile_reports snapshot "$culprit_profile_dir" 3
target/debug/examples/profile_reports replay "$culprit_profile_dir" 3
```

The modes print workload size and mean wall time as JSON. On macOS, prefix an individual measured command with
`/usr/bin/time -l` for peak resident memory. Compare the same file, build profile and iteration count; these local
debug measurements describe the code path and are not release latency guarantees.

`culprit_engine::run_blocking` owns one bounded pool of two workers shared by desktop commands, MCP tools and asynchronous
engine services. Report context, scan history/analysis/persistence, period comparisons, plan parsing, exports and
disk-backed commands use this pool. Queue acquisition has a deadline; cancelled queued calls start no work.
Database futures stay asynchronous; live period statistics, switch-plan reads and recorded changes share an observation
deadline after authentication. Plan context readers release retained evidence before awaiting the database;
period comparisons retain only dates, source and counters across network waits.
Editor linting checks the small latest pointer and releases its snapshot-cache
mutex before analysis. Brief generation reads its global fallback only when the selected window has no saved report.

The desktop, CLI JSON and generated browser demo omit `evidence` from their view projection while keeping provenance;
MCP returns its focused views. Shared exports and digests reparse before
redacting, analyze only redacted input and never copy unredacted collection or recorded events into their replay body.
Saved artifacts and optional metadata remain readable when these additions are absent.

Sharing masks SQL literals, physical paths and endpoint details, including retained measurement and baseline
identities. Database, schema and logical catalog labels remain available for diagnosis. Before masking index predicates,
typed `IndexFilterFacts` retain their opaque equivalence fingerprint and the query ids whose original text established
coverage. Different literal predicates must not become equivalent merely because both now display placeholders;
parameterized or unsupported predicates stay unproven.
Fix SQL rebuilt from redacted evidence keeps generated constants and valid syntax. A finding that cannot be reproduced
by that recheck, including an unavailable external check, gets fallback literal redaction in its SQL as well as prose.
Public deadlock collection limits come from `EngineSpec` when private collector notes are removed, so sharing still
explains the latest-only report or the counter without graphs.

## 3. The pipeline: from a snapshot to a report

`culprit-checks/src/pipeline.rs` runs four stages, in order, each a list you can read top to bottom:

1. **Prepare**: make the snapshot say what the database says now, before anything reads it
   (`pipeline.rs::preparation()`): plans captured before a column changed type, MySQL's possible keys named with their
   columns, plan operators marked with their index's kind from the catalog (a BRIN bitmap counts pages, not rows), the
   health counters' grades held to the evidence rules (`pressure::grade_on_evidence`), PostgreSQL's WAL write and sync
   time kept out of statement-time waits in scans saved before the collector did so (`statement_time_without_wal`), and
   MariaDB's lock time read as unknown where the slow log to a table counts a statement's whole run as it
   (`lock_time_that_counts_the_run`, through `signals::lock_time_counts_the_run`), and what MySQL's runs measured of a
   scan's filter put on the scan (`indexes::mark_measured_kept`), so the plan's own rules judge it as filtered-scan does.
2. **Detect**: every check that applies to the engine and can run with what the login sees (`culprit_checks::all()`).
   A check that needs a missing capability is reported as locked, with the reason, instead of running on missing data.
3. **Refine**: rules over all the findings (`culprit-checks/src/refine.rs`): two findings about one cause become one,
   and a finding that rests on statements whose cost is behind them (they stopped running, or ran once) says so and
   ranks low. A rule here applies to every check on every engine.
4. **Rank and summarize**: actionable findings first (needs attention, then optional improvements), followed by information and history; by severity and impact within each group. Counts and alerts include only actionable findings; then the headline (`summary.rs`).

`culprit_checks::analyze(snapshot)` runs the built-in pipeline. `Pipeline::builtin().with_check(..).with_refine(..)
.with_prepare(..)` adds an embedder's own; they run, rank and summarize exactly like the built-in ones, and a plug-in
check can't take over a built-in id.

### Refinement decisions

The thirty built-in policies live in focused modules under `culprit-checks/src/refine/`. Each selects a structured
decision from the evidence before applying semantic state and composing its explanation. Detectors attach the
cross-finding facts they establish to `Finding::facts`: column lists, a recorded plan transition, lock waits and
write measurements. Human titles, metric labels and sentences do not identify those facts. Wording-only rules have
mutable access to display fields, never rank, actionability, query references or executable SQL.

`Report::refinement` records the rule, affected finding and evidence references, with typed outcomes for history,
withheld fixes, absorption and coverage. Transferring only some queries records a partial transfer and recalculates
the source finding from its remaining evidence. Every removed detector finding must lead to a retained cause;
fixture invariants check this chain. Missing linked queries cannot prove a whole group stopped or redistribute its
measurements to the known subset. Older reports default to empty facts and trace and acquire current decisions when
re-analyzed. An external `Refine` implementation can keep its original interface; the default `refine_traced`
records invocation only. An extension providing its decisions overrides `refine_traced`.

### Checks

A check looks for one kind of problem: `<name>_info()` (id, title, category, the engines it applies to, the
capabilities it needs) and `<name>(&Snapshot) -> Vec<Finding>`. `check!(NameCheck: name_info, name)` makes it a type
implementing `culprit_core::Check`, the same trait a plugged-in check implements, and the area lists it in its `CHECKS`
(`all()` runs every area's, in the order of `checks::AREAS`; `tests/registry.rs` fails when a check is written but not
listed). Each registration also declares its evidence contract, for example
`check!(PlanningTimeCheck: planning_time_info, planning_time, &[Evidence::Query(QueryMeasure::Planning)])`, with
`Evidence` imported as `crate::evidence::Requirement`. The reusable evaluator `evidence::assess` checks sources and
measurements, never check IDs. External checks can call `culprit_checks::evidence::assess` in `Check::evidence` too.
An undeclared contract stays insufficient, and an unavailable mandatory source wins regardless of requirement order.
Partial evidence may still produce a positive finding; it cannot establish a clean assessment. Every check has its
own file, in a directory per area under `culprit-checks/src/checks/` (`queries/`, `indexes/`,
`signals/`, `pressure/`, `maintenance/`, `tuning/`…; `checks/mod.rs` maps them); what a few checks of one area share
sits in that area's `mod.rs`, and what every check shares (the evidence rules) in `support.rs`. A
finding has a stable id (`<check>:<subject>`), a title and a plain-language summary, evidence (metrics, the queries
behind it), a fix that is safe to run, a way to verify it, a severity, a confidence and an impact used for ranking.

Per-statement assessments retain the exact query ids measured, including measured zero WAL, JIT and wait counters.
Wait evidence names its category; another category cannot prove zero lock or network waiting. Query Store zeroes
require a successful wait read and enabled capture. Cached memory grants are sampled for discovery, then the captured
workload's hashes are read separately (at most 200), including zero grants; absence from the cache stays unknown.
A plan regression retains a typed fast-plan baseline in the finding and digest. Recovery may use one later plan,
forced or naturally selected, only with fresh, sufficiently sampled runs inside the new window that no longer meet
the regression threshold. Losing an earlier variant alone never verifies recovery. Source failures, incompatible
measurements, locked checks and refinement into another cause still prevent a fixed verdict.
Legacy source contracts may explicitly accept their retained payload when outcomes did not yet exist. A missing
payload remains unknown, and recorded partial or failed outcomes always win over that fallback.

Cumulative findings declare `VerificationBasis::CounterChange`: losing their counters is unknown, not proof of a fix, and exports ask for comparisons under representative load. This contract applies to built-in and embedded checks alike.

Verify and digest comparisons use `Report::finding_absence_problem` before treating a missing finding as fixed.
A capability lock, an unavailable or incomplete assessment, or a check that did not run cannot establish absence.
Positive findings from a partial source retain their assessment reason, so they cannot clear other observations from
that check. Statement checks also record the identities with complete measurements: an unrelated statement with
missing measurements cannot block verification of a measured statement, and a measured unrelated statement cannot
stand in for the affected one. Saved digests retain those statement identities. A completed assessment or established
inapplicability can confirm absence; older reports without
assessments retain their capability-lock interpretation.
The same guard consults the refinement trace: an observation absorbed or covered by another cause was reclassified,
so losing its old id is not proof of improvement.
Saved digests retain the originating check in `DigestFinding.check`; older digests recover it only from an
unambiguous registered check prefix, so a custom check namespace cannot accidentally borrow another check's result.

Plan insights (`culprit-core/src/insights.rs`) are rules over one plan, shown in the plan viewer; the `plan-insights`
check promotes the ones that matter to findings. `views/plan_advice.rs` applies the selected scan's workload and
catalog to captured and fetched plans: the shared history/materiality rules set urgency, and the index checks'
coverage, filtered-index, INCLUDE and safe-rebuild helpers choose changes. Structural observations remain visible
when cheap or historical; small-table scans keep growth guidance. The adviser and fetched plan variants use the
displayed report's id, and a captured plan is reused only for its recorded `plan_id`.

### The evidence rules: what keeps a finding from being a false flag

Every check and insight follows these. Each has one implementation; use it rather than restating it.

| Rule | Where |
|---|---|
| A rate compared with a threshold needs at least an hour of data; a sentence that says "a day" needs 20 hours, otherwise it says what was counted and over how long. Runs said as they came: a burst is when it ran, in any window of an hour or more ("750 runs on Oct 11, 15:51–15:55 UTC"), runs on a few of the window's days are those days ("120K runs on Oct 11 and Oct 17", from the timeline's buckets), fewer runs than days a count ("1 run in the last 6 days"). A burst is graded on its total and never extrapolated to a day; a statement-level bar a day (chatty's 20K runs, an amount behind a severity) judges it at the rate it ran while it ran. | `RATE_MIN_HOURS`, `PER_DAY_MIN_HOURS`; `support::rate_per_day`, `rate_per_day_of`, `at_least_per_day_of`, `counter_rate_per_day`, `how_many`, `runs_phrase`, `runs_of`; `Workload::cadence`, `rate_per_day_of`, `how_often_of` |
| On a source of running totals (pg_stat_statements, performance_schema, the plan cache), a statement's share, time a day, runs a day and average are what Culprit's own scans measured, over 20 hours or more of them in which it ran, higher or lower than its totals, wherever a person reads them (titles, summaries, evidence, briefs, exports, the dashboard, the digest, MCP, the CLI, the app) and wherever they decide something (impact, grade, rank, which statements are heavy); the totals since the source began counting are said beside them where the two differ twofold, except for a recent entry, whose totals are those runs diluted. A statement that stopped never takes a heavy place of one that runs. A count kept since a reset (pg_stat_statements' evictions) is rated over the scans too, from the oldest a day or more back. A change shows between two scans (What changed), never in the totals, and a rewritten statement is a new entry. What the source keeps only in its totals (a part of a statement's time: its slow runs' excess, a spill, its waits; the rows a run handles) is said over them beside the scans' figures, and each figure (metric labels included) names what it is over, so the two periods aren't read as one. A grade takes its share and its amount on one basis. | `Workload::scans_of`, `statement_share`, `statement_mean_ms`, `statement_time_per_day`, `cost_of`, `how_often_of`; `support::query_impact`, `severity_for_impact`, `severity_for_statement_share`, `amount_severity_of`, `totals_basis`, `scans_sentence`; `queries::heavy_share`, `average_metrics`, `recent_entry`, `verify_between_scans`; `reports::evictions_baseline` |
| A verify step names a number a later scan can show moving, in the direction the fix moves it: a fix that cuts runs (a cache, a skipped or batched call, a pool's reset) expects fewer runs and less time, never fewer reads a run; one that makes each run cheaper, its time a run. On running totals it compares a scan taken when the change ships with a later one, What changed (and `culprit changes <connection> --since`) for runs and time, which is all What changed carries, the two scans' own counts for anything else (temp files, rows, planning time); the brief says the running-totals sentence once. A finding whose statements stopped has nothing to check unless they run again, and no export promises a re-scan shows it gone. | `queries::verify_between_scans`, `verify_counts_between_scans`, `Shows`; refine rule `a finding whose statements stopped has nothing to check`; `brief::gone_after_rescan` |
| No reads, no scans, nothing happened: a claim of absence needs a week of counters (a weekly job has had a chance to run). Counters performance_schema may have reset since the server started start over with its statements: there the week is the statement digests' own counting, never the server's uptime. | `indexes::is_unused`, `watched_long_enough`; `support::watched_days` |
| A statement that ran once, or stopped running, is history: its findings say so, are Low, rank low and get no index of their own. "Stopped" needs 20 hours of silence (and 20 of its usual intervals); under that the finding stays active and says how long it has been quiet. A finding on statements that stopped is told in the past and its fix creates nothing (a missing-index request too). | refine rules `statements that stopped running` / `nothing is created for statements that stopped` / `statements that ran once`; `query_stopped`, `silent_long_enough`, `all_stopped` (a check's tense), `query_is_history` |
| Counters are named for what they cover: performance_schema's start over when its statements are truncated, so where the statements begin well after the server started they are "since the server started or its performance_schema counters were last reset", with no length and no rate a day. "The previous scan" is the scan just before; a baseline further back is "the scan on Oct 15 at 09:32 UTC". | `support::since_phrase`, `counters_may_be_reset`, `counters_measure_a_day` |
| A number reads the same on every surface: the app's `format.ts` mirrors `culprit_core::format` (a tenth under 10 ms, ties rounded to even, a span of days never more than was counted and in hours under one), pinned on shared cases (`app/src/gen/format-cases.json`, checked by `bun run typecheck`). | `culprit_core::format`, `format::days`, `format::days_counted` |
| A captured plan is compared with its own variant's runs and reads, where identified; statement totals across other variants prove no row misestimate for it. Many pages for one row can be large values or index maintenance, so pages alone never prove that a row estimate is wrong, establish a lookup count or justify statistics/index/forced-plan changes. A residual filter beside high page counts needs actual operator measurements first. A handful of runs with one outlier is said as their total and slowest run. Plans retained in the app keep the measurements they were opened with across later scans and connection/window switches. | `QueryStat::captured_plan_variant`, `insights::cheap_plan_runs`, `heavy_query::residual_read`, `brief::plan_run_times`; mirrored by `plan-format.ts`, `plans.ts::addPlan` |
| A unique-key equality lookup's estimate of one row doesn't depend on statistics. Its stored XML alone is no reason to refresh them; statements that do rely on the stale statistics are linked as evidence. | `stale_stats::only_unique_lookups`, `saved_plans_referencing` |
| Tables absent from a collected catalog, whose statements are all history, get no fix against those tables, on every query finding. A missing catalog proves no absence. | refine rule `a table that is gone has nothing to change`, `indexes::table_gone` |
| A claim about a cause is the engine's own verdict, never inferred from a plan's shape ("stops a seek" needs SQL Server's Seek Plan warning). | `insights::implicit_conversion` |
| A saved statistics warning describes compilation time, not current freshness. A seek key does not establish a histogram on that column (SQL Server builds it only on the first statistics key). Current catalog facts stay separate, and verification needs a newly compiled plan. A planner cost default alone justifies no setting change. | `insights::missing_statistics`, `stale_index_statistics_detail`, `plan_warnings`; `config` |
| Call counts do not establish a timer, how many application instances called, or transaction boundaries. Statistics maintenance is named as maintenance, and a valid empty maintenance showplan is absent operator data rather than a parse failure. | `chatty_query`, `statistics_maintenance_table`, `plan_xml::is_empty` |
| A type mismatch is a column the engine converts implicitly to match a value of another type, in the plan that runs now. | `insights::converts_column`, `current_conversions`, `mark_changed_columns` |
| An index is advice only for a filter that keeps a small share of the rows read; past that, the scan is the right plan and the advice is to read less. | `INDEX_KEEPS_AT_MOST`, `PlanNode::kept_share` |
| A setting is changed where the server runs: SQL on a server you manage, the platform's control on a managed one, and never to a lower value than it has. | `support::setting_change` |
| Severity needs an amount, not only a share: a large share of a quiet database's time is still a small number (Medium needs a minute a day of database time, High ten). Time spent since a recent change is rated over its own hours; a window of unknown length is never High. Counts are rated by what they cost (a chatty statement's round trips at about a millisecond each). An average over one or two runs that hides the spread is said as what they took ("one run waited 10.5 s"). | `support::severity_for_time`, `severity_for_time_of`, `severity_for_share_of`, `severity_for_recent_time`, `burst_hours`; `QueryStat::few_runs` |
| A number counted for the whole server (instance waits and compiles, server-wide connections, InnoDB's lock time, another database's consumers) is said as the server's and isn't graded on this database, unless this database's own statements show it. | per check; `pressure::grade_on_evidence` for the health squares |
| An optimizer's default guess isn't an estimate (MySQL's fixed `filtered`, PostgreSQL's default selectivity), and what the runs measured outweighs what the plan estimated. | `insights::mysql_guessed_filtered`, `plan::GUESSED_SELECTIVITY`, `PlanNode::kept_share`, `filtered_scan::measured_keeps_few` |
| Returned-row totals under a LIMIT do not measure its filter: grouped runs can have different limits, and one sample's literal establishes no common cap. Actual operator counts can still establish selectivity. Prepare recomputes its inferred selectivity marks, withdrawing saved marks when today's evidence doesn't support them. | `filtered_scan::measured_keeps_few`, `mark_measured_kept`, `scan_groups` |
| Index keys come only from conditions an index can seek on: a plain column compared with a value. An OR across columns, `<>`, two columns compared, arithmetic or a cast on the column, a hash join's bitmap probe give none. | `insights::seekable_columns`, `support::predicate_columns` |
| Statements a driver or a pool sends (BEGIN, COMMIT, SET, DISCARD ALL, `SELECT 1`) aren't the application's: no N+1, no "find it in the code". | `sql::is_session_statement` |
| A plan insight states a share of the plan's work only where it measured one, under the same rule as a step's share (and its text with it); a rule's own weight is not shown as a share and raises nothing. | `PlanInsight::fixed_weight`, `insights::stated_share` |
| A plan's numbers say what they are: rows read are what the engine reports, or a table's size only for a scan that reads it in full; estimates are compared with actual rows for the runs that happened; a step's share is of what the plan's costs add up to, and none is stated beside a step MySQL puts no cost on, for a step whose cost rests on a guessed estimate (on it, below it, or a loop's outer side: the inner side of a nested loop over `= ANY($1)`), or in a plan that does no real work. The reason for none is the true one: a plan whose costs rest on a guess, whose runs far exceed its estimate or read a whole table, is not "cheap". A whole-table read is said as the reason only where it can matter: a table of `SCAN_ROWS` rows or more (or that many read over the step's runs), not the engine's catalog; all 2 rows of `pg_database` get the plain cheap-plan note. | `PlanNode::rows_read_total`, `reads_table_whole`, `insights::whole_read_matters`, `plan::SCAN_ROWS`, `est_rows_for_runs`, `PlanNode::misestimates`, `Plan::cost_share`, `Plan::step_share`, `Plan::guess`, `no_share_reason`, `plan_shares_note` |
| A plan's step is where a statement's work is only in a plan that does real work (a measured run of 10 ms, or an estimate of what reading about 10K rows costs), and only for runs measured cheap (under 10 ms, a few pages per row, rows near the plan's estimate, no table read whole) is a statement's time "how often it runs". A step's share of the plan is stated only there, on every surface (brief, exports, the plan viewer, the CLI's tree, MCP). Advice for a step (a covering index, a sorted index, an index for a scan) follows the plan's own rule for that step. | `insights::does_real_work`, `working_step`, `runs_cheaply`, `cheap_plan_runs`, `has_insight`; `Plan::step_share`, `no_share_reason`; `filtered_scan::scan_groups` |
| A plan's own missing-index request is a finding unless one for the same table and key exists (the server's list empties on a restart). | refine rule `a plan's index request is the finding for its table and key` |
| No index, statistics or rewrite advice on the engine's own objects (`sys.*`, `pg_catalog`, `information_schema`, `performance_schema`, `sqlite_*`): nothing an application changes. | `insights::on_system_objects` |
| A statement's share is graded as the source allows: where entries count from different moments (the plan cache counts each plan from its compile), a statement claims at most its share of the time an hour, so a plan cached for a month doesn't outweigh one compiled an hour ago by its age. | `Workload::share_of`, `share_weight`, `hourly_time_ms`; `support::query_impact` |
| One cause, one finding. | refine rules |
| A finding's share of database time counts every statement it lists, also once refine folds another finding into it (the share of all of them, not the larger of the two). Recalculation requires complete linked evidence and compatible measurement semantics. | `refine::merge::Absorption`, `share_of_listed` |
| A caveat adds to the finding: no sentence of it repeats the summary or the fix. | refine rule `a caveat repeats nothing the summary or the fix says` |
| An index finding lists a statement only where its fix serves it: one whose captured plan reads the table some other way (a hash join's side, a filter on another column) isn't listed. On MySQL, the rows a statement examined against the table's rows say whether it reads it whole (half or more a run) or through an index (under a tenth). A fix's cost says "this index" only beside the SQL that creates one. | `indexes::through_foreign_key`, `plan_reads_whole`; `scan_heavy::examined_reads_whole`; `indexes::write_cost`, `write_cost_of` |
| An event session a fix starts is one session for every finding that asks for it: created only where it isn't defined, started only where it isn't running, and the SQL says how to drop it. | `signals::event_session_sql`, `event_session_drop` |
| Fix SQL must never damage what exists: rebuilding an index keeps its uniqueness, filter, key order and included columns, or there is no SQL. | per fix |

`measurement.rs` gives measurements a typed contract: database identity, source, scope, period and reset marker.
Providers record the observed database name and routing host/port (or socket directory), or SQLite's canonical file
path and, on Unix, its device/inode. Routing identity does not establish a physical-server UUID: DNS aliases,
proxies and a replacement database at the same endpoint remain limits unless the engine supplies an incarnation.
Credentials never enter the identity. No new database queries are needed to capture it.

Workload periods distinguish selected history, cumulative counters, observed scan intervals, samples and instants.
The requested window does not invent source history; rates use the actual measured duration. A cumulative reading
is dated by collection, not its last query execution. The oldest cache/digest entry is not a source-wide reset.
`series_comparison`, `counter_transition` and `report_comparison` centrally decide which measurements can be compared;
history, query activity, changes, baselines, verification and digests use these contracts. Different endpoints,
sources, scopes or incompatible windows cannot establish a trend or verify a query finding's disappearance.

Saved exports without these optional fields still load. Two older unidentified scans retain an explicit `Legacy`
comparison using their original identity and clock semantics. An identified scan cannot borrow identity for an old
one from today's connection: fresh comparable scans establish new trends. The legacy fields remain serialized for
readers of older report formats; preparation mirrors the typed context into them when context is present.

Cumulative query evidence keeps its dates. `Workload::over_scans` uses complete intervals for shares and averages;
when those intervals have gaps or end before the latest scan, `ScanSpan::phrase` dates them explicitly. A more recent
interval with known query-counter growth but incomplete database totals is separate `StatementActivity`: observed
calls (a lower bound after evictions), never an inferred daily rate, average or share. The UI and briefs read this
same `StatementCost` rather than dividing a new entry's calls by years since the global reset.

Storage counters carry their own `FileIo::scope`, `stats_reset` and `write_operation`. PostgreSQL before 16 measures
data reads per database, while `pg_stat_io` and WAL are shared server sources. WAL syncs are flushes, not writes.
Older scans can recover scope and operation from their provider/version, but an unknown source reset remains unknown.
A reset of database, background-writer or archiver statistics never supplies the WAL source's period.

Health counters follow these rules in `checks/pressure/health.rs`: preparation corrects saved and fresh counters, honors `Snapshot::options` for dated samples, and keeps shared or descriptive ratios ungraded. Azure CPU/I/O/log/worker assessments require 30 minutes of actual 15-second samples with at least 80% coverage per dimension; allocated cache memory is contextual. After refinement, queue and cleanup colors come from final findings. The app's query/table/storage/replica colors also use those findings instead of independent thresholds. Query Store compilation history is exact and separate (`QueryCosts::compile_history`); an old prorated compilation estimate is withdrawn on reread. Cloud episode assessments require contiguous timestamps, preserve the actual fetch period, and are cached per database and period.

### Views built on a report

`culprit-checks/src/views/`: the dashboard (`dashboard/`, one file per tile), the digest for scheduled jobs
(`digest.rs`), the index advisor (`advisor.rs`), the agent brief (`brief.rs`), drift between two databases
(`drift.rs`) and the waits explained (`waits.rs`) read a report and add no findings of their own; they follow the same
evidence rules.

Renderers consume derived views from Rust. `views/plan_view.rs` produces `Plan.view`: normalized executions and row
comparisons, measured operator time, displayable shares, actual-run facts and explanations from the same helpers the
briefs use. Scan preparation, imported plans, on-demand plans and live progress all attach it at their engine boundary.
The projection is output only: deserialization ignores a saved projection, including an older schema, and analysis
rebuilds it from the collected plan. In-flight plans retain their collected counters but get an estimated projection;
partial execution is not a completed measured run. TypeScript maps these facts to layout data without repeating the
analysis. Browser fixtures are generated with these same Rust functions and checked against them.

Plan calculations read `PlanFacts` and `PlanNodeFacts`, defined in `culprit-core/src/plan_facts.rs`. Providers populate
them from native XML, JSON or SQLite plan text; display properties remain available for inspection. Timing, memory,
worker counts, selectivity, execution modes, compiler verdicts, candidate indexes and output columns retain their
units and precision without parsing display labels. Native collection and normalized-plan deserialization treat
invalid numeric evidence as unknown; a measured zero stays distinct from a missing counter. SQL Server's lightweight profiling withdraws its unrecorded timing placeholders at
its collection boundary.

A missing facts object identifies an older normalized plan or a property-only external provider: one compatibility
adapter recovers its inputs. A present object is authoritative, including unknown fields, and never inherits a
conflicting raw property. Preparation updates typed facts and their inspection strings together, withdrawing stale
derived annotations. Catalog enrichment requires an unambiguous table/index identity; an unqualified name shared by
schemas cannot establish one. Column-type context retains schema and target type, and aliases resolve only where
their physical table is known. Redacted exports mask SQL values in typed facts as well as in properties, while
preserving measurements and PostgreSQL's structural SubPlan references.

`dashboard::refresh` applies a live read to an existing dashboard and returns `DashboardLive` (dashboard, sessions,
optional error). Desktop polling calls `Workspace::dashboard_live`, so grades, attention and headlines share the CLI's
implementation. The cold dashboard is pinned to the displayed report, and a reply from a previous connection, report
or selected window is discarded. A failed read clears current measurements; incomplete visibility cannot establish
zero blocked sessions. MCP returns the original numeric signal values, leaving display rounding to its consumer.

## Desktop request lifecycles

`app/src/lib/store.ts` owns UI state and navigation. `report-requests.ts` owns a report's cache → scan lifecycle,
including loading, selection, refresh errors and sidebar refresh. Every open or manual scan gets a distinct owner
from `requests.ts::LatestRequest`, alongside its connection and window. Changing either target invalidates that owner;
returning to the same target never revives an earlier response. Superseded promises may finish, but cannot publish
results or errors, clear a newer spinner, or launch another scan. Connection and saved-query lists have independent
owners. API initialization is shared and retries after rejection. View-local effects retire their callbacks on cleanup.
This ownership governs UI publication; the desktop invocation itself remains bounded by the backend execution policy.

## Extending Culprit

- **A check**: write `<name>_info()` and `<name>(&Snapshot)` in a file of its own in the area's directory under
  `checks/` (declared in the area's `mod.rs`), declare its type with `check!`, add it to the area's `CHECKS` list, give each finding a stable id, follow the evidence rules, and test it on a fixture (`fixtures/*-demo.json`)
  for the case it should flag and the cases it must not.
- **A refine rule**: a `Refine` implementation in the appropriate `refine/` module, listed in `rules()`. Select its
  evidence and outcome first, apply state, then render prose; return the decisions from `refine_traced`. Test both
  the relationship and the incomplete evidence that must leave separate findings.
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
  After changing findings or plan advice, `python3 dev/refresh-demo.py` refreshes the browser's saved reports and all
  their derived findings, dashboard, index advice, check metadata, plan insights and share samples together. A test
  compares the browser's saved output with today's analysis, so golden readings cannot hide stale demo data.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --all`.
