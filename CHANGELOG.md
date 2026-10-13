# Changelog

Each release's notes are its section below: the release workflow publishes them on GitHub, and the desktop app shows them when it offers the update. A release without a section here isn't drafted.

## 0.2.1

### Fixes

- **Microsoft Entra ID sign-in shows one code, once.** Opening a connection asks for a token from several places at once (the report, the dashboard, live polling); each started its own device-code sign-in, so the code on screen kept changing while you typed it, and the sign-ins left waiting held their requests for up to 15 minutes. They now share one sign-in and its token. A sign-in that fails or that you cancel isn't reopened by polling for a minute; `az login` still works straight away.

## 0.2.0

### Updates itself

- **The desktop app keeps itself up to date.** Once a day, in the background and never at start-up's expense, it checks for a newer release and offers it at the bottom of the sidebar. It downloads the update, verifies its signature against the key built into the app, installs it, and restarts into it when you choose. The command palette checks on demand and turns the daily check off.
- **`culprit update` updates the CLI** to the latest release, checking the download against the release's SHA-256 sums. `culprit update --check --json` tells an agent whether a newer release is out. The CLI never checks on its own: an agent that runs `culprit scan` doesn't trigger a network call it didn't ask for.
- **Releases ship signed update packages** for macOS, Windows (the installer), and Linux (the AppImage and the Debian package, each updating itself), plus the bare CLI binaries.

0.1.8 and earlier don't have the updater: install 0.2.0 once by hand, and later releases arrive by themselves.

## 0.1.8

### Findings that match what the database says

- **Lock escalations have their own check, and every escalation is High.** Escalation *attempts*, which SQL Server logs on its own even on idle tables, no longer raise findings.
- **Unused indexes account for their age.** An index that may be less than a week old isn't called unused, overlapping or cleanable, and the schema view says it may be new instead of "never read".
- **Automatic tuning reads as it is.** Plan corrections SQL Server already applied (Verifying, Success) no longer show as suggestions. Forced plans are listed (forced automatically or by hand, since when, and whether the query still runs) with the SQL to unforce them, and a forced plan that scans a large table an index could seek is its own finding.
- **Azure SQL Database's index recommendations show up**, create and drop, with Azure's score and script, merged into the matching missing-index finding.
- **Deadlocks on Azure SQL Database**: `culprit deadlocks` reads the database's deadlock telemetry, which a database user can read.

### Windows in hours

- `--window 6h`, `90m`, `2d` or `1d12h` on `scan`, `dashboard` and `waits`. The app's window picker adds Last hour and Last 6 hours.
- The headline keeps the window you asked for, and says when Query Store's hourly intervals make it cover more.

### App

- **Plan insights copy as Markdown**, all at once from the Insights header or one at a time, with the statement, the fix and how to verify it: ready for a pull request or a coding agent.
