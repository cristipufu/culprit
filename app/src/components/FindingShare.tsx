import { CircleAlert, CircleCheck, ClipboardCopy, FileCode, Info, RotateCw, X } from "lucide-react";
import { useState } from "react";
import type { Finding } from "../gen/Finding";
import type { Report } from "../gen/Report";
import type { VerifiedFinding } from "../gen/VerifiedFinding";
import { api, errorMessage, type ApiError } from "../lib/api";
import { useHotkeys } from "../lib/hotkeys";
import { deliver, exportOptions } from "../lib/share";
import { getState, setState } from "../lib/store";
import { ShareMenu } from "./ShareMenu";

type Result = { status: "running" } | { status: "done"; item: VerifiedFinding; summary: string; newCount: number } | { status: "error"; error: ApiError };

/** "Share" (copy for a PR, save HTML) for the action bar. */
export function FindingShareMenu({ report, finding }: { report: Report; finding: Finding }) {
  const name = report.snapshot.connection;
  return (
    <ShareMenu
      actions={[
        {
          id: "md",
          icon: <ClipboardCopy size={15} />,
          label: "Copy for a pull request",
          hint: "Markdown: what's happening, evidence, fix and how to verify",
          redacts: true,
          run: (redact) => deliver(api().then((c) => c.exportFinding(name, finding.id, "markdown", exportOptions(redact))), "copy", redact),
        },
        {
          id: "html",
          icon: <FileCode size={15} />,
          label: "Save as an HTML page",
          hint: "One file with the query's plan; opens offline, prints cleanly",
          redacts: true,
          run: (redact) => deliver(api().then((c) => c.exportFinding(name, finding.id, "html", exportOptions(redact))), "save", redact),
        },
      ]}
    />
  );
}

/** "Verify fix": re-scan and say whether this finding is gone, without leaving it. Shortcut V. */
export function useVerify(report: Report, finding: Finding) {
  const [result, setResult] = useState<Result | null>(null);
  const run = async () => {
    if (result?.status === "running") return;
    setResult({ status: "running" });
    try {
      const v = await (await api()).verifyFindings(report.snapshot.connection, [finding.id]);
      const item = v.findings[0];
      if (!item) throw new Error("The scan didn't return this finding's status");
      setResult({ status: "done", item, summary: v.summary, newCount: v.new_findings.length });
    } catch (e) {
      setResult({ status: "error", error: errorMessage(e) });
    }
  };
  useHotkeys({ v: run });
  return { result, run, clear: () => setResult(null) };
}

export function VerifyButton({ running, onRun }: { running: boolean; onRun: () => void }) {
  return (
    <button className="btn" onClick={onRun} disabled={running} title="Re-scan and check whether this finding is gone">
      {running ? <span className="spinner" style={{ width: 12, height: 12 }} /> : <RotateCw size={13} />} {running ? "Re-scanning…" : "Verify fix"}
      {!running && <span className="kbd">V</span>}
    </button>
  );
}

/** The verification answer, above the action bar. */
export function VerifyResult({ result, report, onClose }: { result: Result | null; report: Report; onClose: () => void }) {
  if (!result || result.status === "running") return null;
  if (result.status === "error") {
    return (
      <div className="verify-card neutral" role="status">
        <Info size={16} />
        <div>
          <b>{result.error.message}</b>
          {result.error.hint && <div className="muted">{result.error.hint}</div>}
        </div>
        <button className="btn icon ghost" aria-label="Close" onClick={onClose}>
          <X size={13} />
        </button>
      </div>
    );
  }
  const { item, newCount } = result;
  const tone = item.status === "fixed" ? "good" : item.status === "still-present" ? "warn" : "neutral";
  const title =
    item.status === "fixed" ? "Fixed. The new scan no longer reports it." : item.status === "still-present" ? "Still present in the new scan." : "Can't tell from the new scan.";
  const showNewScan = async () => {
    const name = report.snapshot.connection;
    const fresh = await (await api()).latestReport(name, getState().windowMinutes);
    if (!fresh || getState().active !== name) return;
    const keep = getState().finding;
    setState({
      report: { status: "ready", report: fresh, scanning: false, scanError: null },
      finding: keep && fresh.findings.some((f) => f.id === keep) ? keep : fresh.findings[0]?.id ?? null,
    });
    onClose();
  };
  return (
    <div className={`verify-card ${tone}`} role="status">
      {item.status === "fixed" ? <CircleCheck size={16} /> : item.status === "still-present" ? <CircleAlert size={16} /> : <Info size={16} />}
      <div>
        <b>{title}</b>
        {item.before && item.after && item.before !== item.after && (
          <span className="muted">
            {" "}
            {item.before} → {item.after}
          </span>
        )}
        {item.note && <div className="muted">{item.note}</div>}
        {newCount > 0 && (
          <div className="muted">
            The scan also found {newCount} new finding{newCount === 1 ? "" : "s"}.
          </div>
        )}
        <button className="link" onClick={showNewScan}>
          Show the new scan
        </button>
      </div>
      <button className="btn icon ghost" aria-label="Close" onClick={onClose}>
        <X size={13} />
      </button>
    </div>
  );
}
