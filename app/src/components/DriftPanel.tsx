import { ArrowLeft, Check, Copy, SquarePen } from "lucide-react";
import { useEffect, useState } from "react";
import type { Drift } from "../gen/Drift";
import type { DriftItem } from "../gen/DriftItem";
import type { DriftKind } from "../gen/DriftKind";
import type { DriftSide } from "../gen/DriftSide";
import type { Report } from "../gen/Report";
import { api, errorMessage, type ApiError } from "../lib/api";
import { ago } from "../lib/format";
import { setState, useStore } from "../lib/store";
import { Sql } from "./Sql";
import { serverName } from "../lib/engines";
import { copyAndTell } from "../lib/clipboard";
import { canOpenFixInEditor } from "../lib/editor-sql";

type DriftState = { status: "idle" } | { status: "loading" } | { status: "ready"; drift: Drift } | { status: "error"; error: ApiError };

const GROUPS: { kind: DriftKind; label: string }[] = [
  { kind: "index", label: "Indexes" },
  { kind: "table", label: "Tables" },
  { kind: "column", label: "Columns" },
  { kind: "setting", label: "Settings" },
  { kind: "version", label: "Version" },
];

function pickKey(name: string): string {
  return `culprit.drift.${name}`;
}

function remembered(name: string): string | null {
  try {
    return localStorage.getItem(pickKey(name));
  } catch {
    return null;
  }
}

/** What differs between this connection's latest scan and another connection of the same engine. */
export function DriftPanel({ report, onClose }: { report: Report; onClose: () => void }) {
  const active = useStore((s) => s.active) ?? report.snapshot.connection;
  const connections = useStore((s) => s.connections);
  const others = connections.filter((c) => c.engine === report.snapshot.server.engine && c.name !== active);
  const [other, setOther] = useState<string | null>(() => {
    const last = remembered(active);
    return others.find((c) => c.name === last)?.name ?? others[0]?.name ?? null;
  });
  const [state, setDrift] = useState<DriftState>({ status: "idle" });

  useEffect(() => {
    if (!other) return;
    let cancelled = false;
    setDrift({ status: "loading" });
    api()
      .then((client) => client.drift(active, other))
      .then((drift) => !cancelled && setDrift({ status: "ready", drift }))
      .catch((e) => !cancelled && setDrift({ status: "error", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [active, other]);

  const choose = (name: string) => {
    setOther(name);
    try {
      localStorage.setItem(pickKey(active), name);
    } catch {}
  };

  return (
    <div className="drift">
      <div className="toolbar">
        <button className="btn ghost" onClick={onClose}>
          <ArrowLeft size={13} /> Schema
        </button>
        <span className="drift-with">
          Compare <b>{active}</b> with
        </span>
        {others.map((c) => (
          <button key={c.name} className={`chip ${other === c.name ? "on" : ""}`} onClick={() => choose(c.name)}>
            <span className={`env-dot env-${c.environment}`} style={{ width: 6, height: 6, boxShadow: "none" }} /> {c.name}
          </button>
        ))}
      </div>
      <div className="drift-body">
        {others.length === 0 && (
          <div className="empty" style={{ padding: 40 }}>
            <h3>Nothing to compare with yet</h3>
            <p>Add another {serverName(report.snapshot.server.engine, report.snapshot.server.product)} connection, such as staging or production, and scan it. Culprit then shows the indexes, columns and settings that differ.</p>
          </div>
        )}
        {state.status === "loading" && (
          <div className="empty" style={{ padding: 40 }}>
            <div className="spinner" />
            Comparing the latest scans…
          </div>
        )}
        {state.status === "error" && (
          <div className="notice" style={{ margin: 20 }}>
            <div>
              <b>{state.error.message}</b>
              {state.error.hint && <div className="muted">{state.error.hint}</div>}
            </div>
          </div>
        )}
        {state.status === "ready" && <DriftReport drift={state.drift} active={active} />}
      </div>
    </div>
  );
}

/** Product and version without repeating the version ("PostgreSQL 18.6 (Debian …)"). */
function serverLabel(side: DriftSide): string {
  const [first = "", ...rest] = side.version.split(" ");
  if (!side.product.endsWith(first)) return `${side.product} ${side.version}`;
  return rest.length ? `${side.product} ${rest.join(" ")}` : side.product;
}

function SideCard({ side, letter }: { side: DriftSide; letter: string }) {
  return (
    <div className="drift-side">
      <div className="name">
        <span className="drift-letter">{letter}</span>
        <span className={`env-dot env-${side.environment}`} style={{ width: 6, height: 6, boxShadow: "none" }} />
        <b>{side.connection}</b>
      </div>
      <div className="muted">
        {serverLabel(side)} · scanned {ago(side.taken_at)}
      </div>
    </div>
  );
}

function DriftReport({ drift, active }: { drift: Drift; active: string }) {
  return (
    <div className="drift-report">
      <div className="drift-sides">
        <SideCard side={drift.a} letter="A" />
        <SideCard side={drift.b} letter="B" />
      </div>
      <p className={`drift-summary ${drift.items.length === 0 ? "same" : ""}`}>
        {drift.items.length === 0 && <Check size={14} />} {drift.summary}
      </p>
      {GROUPS.map(({ kind, label }) => {
        const items = drift.items.filter((i) => i.kind === kind);
        if (items.length === 0) return null;
        return (
          <section key={kind}>
            <h3>
              {label} <span className="muted">{items.length}</span>
            </h3>
            {items.map((item) => (
              <DriftRow key={`${item.kind}:${item.subject}:${item.presence}`} item={item} drift={drift} active={active} />
            ))}
          </section>
        );
      })}
    </div>
  );
}

/** Short badge text (the side cards say which connection is A and B) and the full wording for the tooltip. */
function presenceLabel(item: DriftItem, drift: Drift): [string, string] {
  switch (item.presence) {
    case "only-a":
      return ["only in A", `Only in ${drift.a.connection}`];
    case "only-b":
      return ["only in B", `Only in ${drift.b.connection}`];
    case "different":
      return ["different", "Present in both, defined differently"];
    case "renamed":
      return ["renamed", "Same definition under a different name"];
  }
}

function DriftRow({ item, drift, active }: { item: DriftItem; drift: Drift; active: string }) {
  const compare = item.presence === "different" || item.presence === "renamed";
  const [badge, tooltip] = presenceLabel(item, drift);
  const definition = item.presence === "only-a" ? item.a : item.presence === "only-b" ? item.b : null;
  const copy = async () => {
    if (!item.sql) return;
    await copyAndTell(item.sql, "SQL copied");
  };
  return (
    <div className={`drift-item ${item.presence}`}>
      <span className={`drift-badge ${item.presence}`} title={tooltip}>
        {badge}
      </span>
      <div className="drift-main">
        <div className="drift-text">{item.title}</div>
        {definition && <code className="drift-def">{definition}</code>}
        {compare && (item.a || item.b) && (
          <div className="drift-values">
            <span className="drift-letter">A</span>
            <code>{item.a ?? "—"}</code>
            <span className="drift-letter">B</span>
            <code>{item.b ?? "—"}</code>
          </div>
        )}
        {item.sql && (
          <div className="drift-sql">
            <div className="drift-sql-head">
              <span className="muted">
                To align, on <b>{item.sql_for}</b>
              </span>
              <span className="actions">
                <button className="btn ghost" onClick={copy}>
                  <Copy size={12} /> Copy
                </button>
                {item.sql_for === active && canOpenFixInEditor(item.sql, drift.a.engine) && (
                  <button className="btn ghost" onClick={() => setState({ view: "editor", editorSql: item.sql })}>
                    <SquarePen size={12} /> Open in editor
                  </button>
                )}
              </span>
            </div>
            <div className="codeblock">
              <Sql text={item.sql} pretty={false} />
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
