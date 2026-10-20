import { Check, ChevronRight, Info } from "lucide-react";
import { useState } from "react";
import type { Report } from "../gen/Report";
import { engineLimitations } from "../lib/engines";

/** The pipeline's evidence assessment. No finding alone never means fine. */
export function CheckedFine({ report }: { report: Report }) {
  const [open, setOpen] = useState(report.findings.length <= 3);
  const assessments = report.assessments ?? [];
  const assessed = assessments.filter((c) => c.status === "assessed");
  const limited = assessments.filter((c) => c.status === "insufficient-evidence" || c.status === "unavailable" || (c.status === "findings" && c.reason));
  const inapplicable = assessments.filter((c) => c.status === "not-applicable");
  if (!assessments.length) return <p className="muted">This saved report has no check coverage assessment. Scan again to see which problems could be assessed.</p>;
  return (
    <div className="checked-fine">
      <button className="checked-head" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
        <ChevronRight size={13} className={open ? "turned" : ""} />
        <span>Check coverage <span className="n">{assessed.length} assessed without a finding · {limited.length} limited</span></span>
      </button>
      {open && <>
        {assessed.length > 0 && <ul>{assessed.map((c) => <li key={c.check}>
          <Check size={13} className="ok" /><span><span className="t">{c.title}</span><span className="d">No finding from the measurements available in this scan.</span></span>
        </li>)}</ul>}
        {limited.length > 0 && <details><summary>Limited evidence · {limited.length}</summary><p className="muted">These checks could not establish a negative result. Any supported observations remain in Findings.</p><ul>{limited.map((c) => <li key={c.check}>
          <Info size={13} /><span><span className="t">{c.title} · {c.status === "unavailable" ? "Unavailable" : "Insufficient evidence"}</span><span className="d">{c.reason}</span></span>
        </li>)}</ul></details>}
        {inapplicable.length > 0 && <details><summary>Not applicable · {inapplicable.length}</summary><ul>{inapplicable.map((c) => <li key={c.check}><span><span className="t">{c.title}</span><span className="d">{c.reason}</span></span></li>)}</ul></details>}
        <details><summary>What these sources cannot establish</summary><ul>{engineLimitations(report.snapshot.server.engine).map((text) => <li key={text}><Info size={13} /><span>{text}</span></li>)}</ul></details>
      </>}
    </div>
  );
}
