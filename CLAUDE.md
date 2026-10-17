# Working on Culprit

Read `docs/DESIGN.md` first: it is the source of truth for UX and behaviour. `docs/ARCHITECTURE.md` is the map of the code: sources → snapshot → prepare → detect → refine → rank, and the evidence rules every check follows so a finding is never a false flag.

## Layout and rules

- `crates/culprit-core` owns the model. Every type the UI or CLI shows derives `ts_rs::TS`; regenerate the TypeScript with `cargo test -p culprit-core -p culprit-engine` (writes `app/src/gen/`). Never hand-edit `app/src/gen`.
- Checks (`crates/culprit-checks`) are pure functions of a `Snapshot`. They never touch a connection. A new check: add `<name>_info()` + `<name>(&Snapshot)` in a file of its own in the area's directory under `crates/culprit-checks/src/checks/` (`checks/mod.rs` maps the areas) declare its type with `check!(NameCheck: name_info, name)` (the same `culprit_core::Check` an embedding application plugs in), and add `&NameCheck` to that area's `CHECKS` list; `lib.rs::all()` runs every area's (a test fails if a check is written but not listed). Views built on a report (dashboard, advisor, digest, brief, drift) live in `src/views/`. Findings need a human title, a plain-language summary, a fix and a way to verify it. Give each finding a stable id (`<check>:<subject>`).
- The analysis is a pipeline (`crates/culprit-checks/src/pipeline.rs`): prepare the snapshot, run the checks, refine across all findings (`refine.rs`: one cause → one finding; statements that stopped or ran once are history), rank. A rule that applies to every check belongs in refine or prepare, once, not in each check.
- Follow the evidence rules in `docs/ARCHITECTURE.md` and use their one implementation: rates through `support::rate_per_day` / `counter_rate_per_day` (≥ 1 h of data; say "a day" only over 20 h; on a source of running totals, over Culprit's own scans: `Workload::over_scans`, `how_often_of`), absence (unused, never scanned) only after a week of counters, the engine's own verdict over inference from a plan's shape, index advice only under `INDEX_KEEPS_AT_MOST`, settings through `support::setting_change`, and fix SQL that never drops what exists.
- Checks plug in from outside without forking: implement `culprit_core::Check` and pass it to `culprit_checks::analyze_with(snapshot, &extra)` or `Workspace::open()?.with_check(Arc::new(MyCheck))`. Plugged-in checks run, rank, de-duplicate, summarize and lock exactly like built-ins; ids must be unique (a clash with a built-in id is ignored). See `crates/culprit-checks/tests/plugin.rs`.
- What Culprit knows about an engine, apart from reading it, is one file in `crates/culprit-core/src/engines/` (`EngineSpec`: label, abilities, SQL dialect, settings and statistics advice, refusals); checks and providers ask `engine.spec()` instead of matching the engine, and the app reads the same facts from `app/src/gen/engines.json`. A new engine adds a file there plus its provider crate.
- Engines are compile-time plugins (feature-gated provider crates behind `registry.rs`); an embedder can also supply a provider at runtime with `Workspace::with_provider(engine, factory)` (a wrapped driver, a remote collector, a test double). `crates/culprit-engine/tests/plugin.rs` runs a full scan through one. `Workspace::open_in(dir)` keeps state out of the global config.
- Providers (`crates/culprit-mssql`, `crates/culprit-postgres`, `crates/culprit-mysql`, `crates/culprit-sqlite`) collect snapshots from their sources. A file engine (SQLite) is opened read-only and needs nothing from the application (`docs/providers/sqlite.md`); a long read of the whole file is bounded so the application's writers never wait on it, and its statements come only from a log the framework already writes (`crates/culprit-querylog`). Collectors must be safe on production: statistics and catalog views only. Each provider declares its sources in `src/sources.rs` (`culprit_core::Source`; `culprit sources` lists them) and reads each through `Coverage::read`: a failing source becomes a note and its capabilities locked with why, never a failed scan. Tag every statement Culprit sends with `/* culprit */` so it is excluded from the workload. A snapshot describes one database: filter server-wide views to it, or mark them shared and don't grade them. A number the engine doesn't keep is `None`, never 0. A provider's `reparse` reads a saved plan again from its text, so a saved scan loads with today's parser (`culprit_engine::plans::reparse_saved`).
- `crates/culprit-engine` is the only place that touches secrets (keychain, `--password-env`, Entra tokens via `az`). Secrets are never serialized, logged or included in errors.
- The UI (`app/src`) renders database-sourced text as text only. No `dangerouslySetInnerHTML`.

## Commands

```sh
cargo test --workspace                          # a crate's tests/*.rs are modules of one binary: list a new file in tests/all.rs
cargo test -p culprit-checks --test all brief:: # one test file
cargo clippy --workspace --all-targets
cargo fmt --all
cd app && bun run typecheck && bun run dev     # browser UI on fixtures/*.report.json
cd app && bunx tauri dev                        # desktop app
```

Demo databases with planted problems: `dev/postgres` (Docker, port 55432) and `dev/mssql` (Docker, port 14333). Regenerate UI fixtures with `culprit analyze <snapshot.json> --connection <name> --json > fixtures/<name>.report.json`.
