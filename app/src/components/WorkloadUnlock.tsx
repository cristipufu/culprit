import { Copy, KeyRound, RefreshCw, SquarePen } from "lucide-react";
import type { MissingCapability } from "../gen/MissingCapability";
import type { Report } from "../gen/Report";

import { scan, setState } from "../lib/store";
import { keys } from "../lib/keys";
import { copyAndTell } from "../lib/clipboard";

/** The capabilities that hold query statistics, per engine. Without one, findings come from tables and indexes only. */
const WORKLOAD = ["pg-stat-statements", "query-store", "plan-cache", "performance-schema"];

const SOURCE: Record<string, string> = {
  "pg-stat-statements": "pg_stat_statements",
  "query-store": "Query Store",
  "plan-cache": "the plan cache",
  "performance-schema": "performance_schema statement digests or the slow query log table",
};

/** The first SQL statement in a fix sentence ("Run CREATE EXTENSION pg_stat_statements; in this database…"). */
function fixSql(fix: string | null): string | null {
  const match = fix?.match(/\b(CREATE|GRANT|ALTER|EXEC|SET|UPDATE|CALL)\b[^;]*;/i);
  return match ? match[0] : null;
}

/** The fix sentence with the statement taken out, so the card shows the SQL once. */
function fixText(fix: string, sql: string | null): string {
  if (!sql) return fix;
  return fix
    .replace(sql, "")
    .replace(/^\s*Run\s+(in this database\.?\s*)?/i, "")
    .replace(/\s{2,}/g, " ")
    .trim();
}

/**
 * Shown when the database keeps no query statistics Culprit can read. Says what that costs, what one step unlocks it,
 * and offers the exact statement: copy it, or open it in the editor (which asks before it changes a production database).
 */
export function WorkloadUnlock({ report }: { report: Report }) {
  if (report.snapshot.workload) return null;
  const missing: MissingCapability | undefined = report.snapshot.server.missing.find((m) => WORKLOAD.includes(m.capability));
  if (!missing) return null;
  const sql = fixSql(missing.fix);
  const rest = missing.fix ? fixText(missing.fix, sql) : null;
  return (
    <div className="unlock">
      <div className="unlock-head">
        <KeyRound size={15} />
        <b>Turn on query statistics</b>
      </div>
      <p>
        {missing.reason} Until then Culprit can only check tables, indexes and settings: no query list, no plans, no trends.
      </p>
      {sql && (
        <div className="unlock-sql">
          <code>{sql}</code>
          <span className="spacer" />
          <button className="btn small" onClick={() => copyAndTell(sql, "Copied")} aria-label="Copy the statement">
            <Copy size={12} /> Copy
          </button>
          <button className="btn small" onClick={() => setState({ view: "editor", editorSql: sql })} aria-label="Open the statement in the editor">
            <SquarePen size={12} /> Open in editor
          </button>
        </div>
      )}
      {rest && <p className="muted">{rest}</p>}
      <div className="unlock-foot">
        <span className="muted">
          {SOURCE[missing.capability] ? `Culprit reads ${SOURCE[missing.capability]} only; it never changes the database for you.` : ""}
        </span>
        <button className="btn small" onClick={() => scan(report.snapshot.connection)} title={keys("Scan again once it's on (⌘R)")}>
          <RefreshCw size={12} /> Done, re-scan
        </button>
      </div>
    </div>
  );
}
