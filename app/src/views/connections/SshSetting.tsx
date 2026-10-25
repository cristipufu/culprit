import { Cable, Check, X } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { ConnectionInfo } from "../../gen/ConnectionInfo";
import { sshConfig, sshDraft, type SshDraft } from "../../lib/ssh";
import { api, errorMessage, type ApiError } from "../../lib/api";
import { useFocusTrap } from "../../lib/focus";
import { LatestRequest } from "../../lib/requests";
import { getState, setState, toast } from "../../lib/store";
import { SshFields } from "./SshFields";

export function SshSetting({ connection }: { connection: ConnectionInfo }) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const close = () => { setOpen(false); trigger.current?.focus(); };
  return <>
    <button ref={trigger} type="button" className={`btn ghost conn-ssh ${connection.ssh ? "on" : ""}`} onClick={() => setOpen(true)}
      aria-label={`SSH settings for ${connection.name}`} title={connection.ssh ? `SSH via ${connection.ssh.destination}` : "Configure an SSH jump host"}>
      <Cable size={12} /> SSH{connection.ssh && <Check size={10} />}
    </button>
    {open && <SshDialog key={connection.name} connection={connection} onClose={close} />}
  </>;
}

type Status = { kind: "idle" } | { kind: "testing" | "saving" } | { kind: "ok"; label: string } | { kind: "error"; error: ApiError };

function SshDialog({ connection, onClose }: { connection: ConnectionInfo; onClose: () => void }) {
  const [draft, setDraft] = useState<SshDraft | null>(() => sshDraft(connection.ssh));
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const root = useRef<HTMLFormElement>(null);
  const close = useRef<HTMLButtonElement>(null);
  const action = useRef(new LatestRequest());
  const title = useId();
  const saving = status.kind === "saving";
  useFocusTrap(root);
  useEffect(() => { close.current?.focus(); return () => action.current.invalidate(); }, []);
  const test = async () => {
    if (!root.current?.reportValidity()) return;
    const current = action.current.start(); setStatus({ kind: "testing" });
    try {
      const server = await (await api()).testSsh(connection.name, sshConfig(draft));
      if (current()) setStatus({ kind: "ok", label: `${server.product} · ${server.database}` });
    } catch (error) { if (current()) setStatus({ kind: "error", error: errorMessage(error) }); }
  };
  const save = async () => {
    if (!root.current?.reportValidity()) return;
    const current = action.current.start(); setStatus({ kind: "saving" });
    try {
      const saved = await (await api()).setSsh(connection.name, sshConfig(draft));
      if (!current()) return;
      setState({ connections: getState().connections.map((c) => c.name === connection.name ? { ...c, ssh: saved.ssh } : c) });
      if (current()) { toast(draft ? `SSH settings saved for ${connection.name}` : `SSH disabled for ${connection.name}`); onClose(); }
    } catch (error) { if (current()) setStatus({ kind: "error", error: errorMessage(error) }); }
  };
  return createPortal(<div className="ssh-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget && !saving) onClose(); }}>
    <form ref={root} className="ssh-dialog" role="dialog" aria-modal="true" aria-labelledby={title} onSubmit={(event) => { event.preventDefault(); if (!saving) void save(); }} onKeyDown={(event) => {
      event.stopPropagation();
      if (event.key === "Escape") { event.preventDefault(); if (!saving) onClose(); }
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") event.preventDefault();
    }}>
      <header className="ssh-dialog-head"><div><h2 id={title}>SSH jump host</h2><p>{connection.name}</p></div><button ref={close} type="button" className="icon-btn" disabled={saving} aria-label="Close SSH settings" onClick={onClose}><X size={17} /></button></header>
      <div className="ssh-endpoint"><span>Database</span><span>{connection.display}</span></div>
      <SshFields value={draft} disabled={saving} onChange={(value) => { action.current.invalidate(); setDraft(value); setStatus({ kind: "idle" }); }} />
      {status.kind === "error" && <div className="notice error" role="alert"><div><b>{status.error.message}</b>{status.error.hint && <div>{status.error.hint}</div>}</div></div>}
      {status.kind === "ok" && <div className="ssh-tested"><Check size={14} /> Connected · {status.label}</div>}
      <footer className="ssh-dialog-actions"><button type="button" className="btn" disabled={saving || status.kind === "testing"} onClick={() => void test()}>{status.kind === "testing" ? "Testing…" : "Test connection"}</button><span className="spacer" /><button type="button" className="btn ghost" disabled={saving} onClick={onClose}>Cancel</button><button type="submit" className="btn primary" disabled={saving}>{saving ? "Saving…" : "Save settings"}</button></footer>
    </form>
  </div>, document.body);
}
