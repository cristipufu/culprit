# Working on Culprit

Read `docs/DESIGN.md` first: it is the source of truth for UX and behaviour.

## Layout and rules

- `crates/culprit-core` owns the model. Every type the UI or CLI shows derives `ts_rs::TS`; regenerate the TypeScript with `cargo test -p culprit-core -p culprit-engine` (writes `app/src/gen/`). Never hand-edit `app/src/gen`.
- Checks (`crates/culprit-checks`) are pure functions of a `Snapshot`. They never touch a connection. A new check: add `<name>_info()` + `<name>(&Snapshot)` in the right module and register it in `lib.rs::all()`. Findings need a human title, a plain-language summary, a fix and a way to verify it. Give each finding a stable id (`<check>:<subject>`).
- Checks plug in from outside without forking: implement `culprit_core::Check` and pass it to `culprit_checks::analyze_with(snapshot, &extra)` or `Workspace::open()?.with_check(Arc::new(MyCheck))`. Plugged-in checks run, rank, de-duplicate, summarize and lock exactly like built-ins; ids must be unique (a clash with a built-in id is ignored). See `crates/culprit-checks/tests/plugin.rs`.
- Providers (`crates/culprit-mssql`, `crates/culprit-postgres`) collect snapshots. Collectors must be safe on production: statistics and catalog views only; a failing collector becomes a `CollectorNote` + `MissingCapability`, never a failed scan. Tag every statement Culprit sends with `/* culprit */` so it is excluded from the workload.
- `crates/culprit-engine` is the only place that touches secrets (keychain, `--password-env`, Entra tokens via `az`). Secrets are never serialized, logged or included in errors.
- The UI (`app/src`) renders database-sourced text as text only. No `dangerouslySetInnerHTML`.

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
cd app && bun run typecheck && bun run dev     # browser UI on fixtures/*.report.json
cd app && bunx tauri dev                        # desktop app
```

Demo databases with planted problems: `dev/postgres` (Docker, port 55432) and `dev/mssql` (Docker, port 14333). Regenerate UI fixtures with `culprit analyze <snapshot.json> --connection <name> --json > fixtures/<name>.report.json`.
