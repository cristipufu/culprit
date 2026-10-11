import { Check, ChevronRight } from "lucide-react";
import { useEffect, useState } from "react";
import type { CheckInfo } from "../gen/CheckInfo";
import type { Report } from "../gen/Report";
import { api } from "../lib/api";

let cached: Promise<CheckInfo[]> | null = null;

function allChecks(): Promise<CheckInfo[]> {
  cached ??= api()
    .then((client) => client.checks())
    .catch(() => []);
  return cached;
}

/**
 * The checks that ran on this database and found nothing, so "no findings" reads as an answer with evidence rather
 * than an empty screen. Open by default when there is little else to show.
 */
export function CheckedFine({ report }: { report: Report }) {
  const [checks, setChecks] = useState<CheckInfo[] | null>(null);
  const [open, setOpen] = useState(report.findings.length <= 3);

  useEffect(() => {
    let alive = true;
    void allChecks().then((list) => alive && setChecks(list));
    return () => {
      alive = false;
    };
  }, []);

  if (!checks) return null;
  const engine = report.snapshot.server.engine;
  const locked = new Set(report.locked.map((l) => l.check.id));
  const found = new Set(report.findings.map((f) => f.check));
  const fine = checks.filter((c) => c.engines.includes(engine) && !locked.has(c.id) && !found.has(c.id));
  if (fine.length === 0) return null;

  return (
    <div className="checked-fine">
      <button className="checked-head" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
        <ChevronRight size={13} className={open ? "turned" : ""} />
        Checked and fine <span className="n">{fine.length}</span>
      </button>
      {open && (
        <ul>
          {fine.map((c) => (
            <li key={c.id} title={c.description}>
              <Check size={13} className="ok" />
              <span>
                <span className="t">{c.title}</span>
                <span className="d">{c.description}</span>
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
