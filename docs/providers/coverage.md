# Evidence and provider coverage

[Engine parity](../parity.md) is the coverage index. Each provider page names its own sources, permissions, measurements
and limits. These rules govern how Culprit interprets evidence across every provider.

## Availability and scope

A missing, denied, failed or truncated source records its own coverage outcome. It does not become a zero value or
prove that a database is healthy. Findings distinguish a measured negative result from missing evidence.

Each report's `assessments` distinguishes findings, assessed checks without a finding, insufficient evidence,
unavailable sources and inapplicable checks. Supported findings can coexist with incomplete evidence. The app's
Check coverage panel and CLI JSON retain the reasons; older reports without assessments need fresh analysis.

A scan describes one database. Server-wide or shared-node measurements stay labeled with that scope and are not
charged to one database or statement. Restricted role visibility and incomplete contributors remain explicit.

## Periods and comparisons

Current allocation, ages and queues describe scan time. Historical totals retain their dates and do not become
selected-window counters. Retained events, query statistics, plan caches and sampled task activity can cover different
periods; Culprit does not substitute one for another.

A counter comparison needs matching identity, scope and reset, monotonically increasing counts, and scans in
chronological order. Distributed counters also need the same complete set of contributing nodes. Without that
evidence, an observation remains context rather than a measured change.

Rates require sufficient dated history. Absence claims such as an index never being read require at least a week of
relevant counters, not merely the time since Culprit was installed or a database was reopened.

## Plans and findings

An estimated or compilation plan is not a trace of an actual execution. Operator timing, measured rows, trigger work
and historical parameter values require the source that recorded them or an explicitly requested measured run.
Trigger timing can overlap its modifying operator and is not blindly added to the tree.

Where the engine supplies its own verdict, that verdict takes precedence over an inference from plan shape. A scan,
ratio, queue, allocation or optimizer cost alone does not establish a bottleneck. Findings state the data period,
confidence, fix and verification, and unavailable evidence prevents an unsupported conclusion.

Query and plan recommendations preserve result semantics, including NULLs, ordering and pagination. Verify equivalent
results, reads and duration under representative data; a changed operator alone is not enough. An inexpensive run can
still expose a scaling risk without establishing an urgent index migration.

## Index availability and maintenance

An index's availability to the optimizer and whether writes maintain it are different facts. The provider supplies
those states. Failed schema reads cannot justify a recommendation based on an absent index, and unknown usage cannot
justify dropping an index as unused.

Advice uses the engine's dialect and the actual table kind, version and recorded definition. Proposed changes are
shown for a person or embedding application to review; scans do not apply them.

## Evidence beyond a scan

Backup recoverability, corruption, filesystem capacity, application transaction boundaries and distributed request
traces need their own evidence. Culprit does not report these areas healthy from their absence. Provider pages explain
which related observations are available and what they cannot establish.

For the implementation and source contracts, see the [architecture](../ARCHITECTURE.md).
