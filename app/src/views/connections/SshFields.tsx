import { Cable, FolderOpen } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import type { SshDraft } from "../../lib/ssh";
import { api, errorMessage, isDesktop } from "../../lib/api";
import { LatestRequest } from "../../lib/requests";
import "./ssh.css";

/** One optional route, shared by new connections and saved connection settings. No SSH secret enters the UI. */
export function SshFields({ value, onChange, disabled = false }: {
  value: SshDraft | null; onChange: (value: SshDraft | null) => void; disabled?: boolean;
}) {
  const id = useId();
  const latest = useRef(value); latest.current = value;
  const request = useRef(new LatestRequest());
  const [advanced, setAdvanced] = useState(!!value?.port || !!value?.identityFile);
  const [picking, setPicking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => () => request.current.invalidate(), []);
  const chooseKey = async () => {
    const current = request.current.start(); setPicking(true); setError(null);
    try {
      const path = await (await api()).chooseSshIdentity();
      if (current() && path && latest.current) onChange({ ...latest.current, identityFile: path });
    } catch (error) { if (current()) setError(errorMessage(error).message); }
    finally { if (current()) setPicking(false); }
  };
  return <fieldset className={`ssh-fields ${value ? "enabled" : ""}`} disabled={disabled}>
    <label className="ssh-toggle">
      <input type="checkbox" checked={value !== null} onChange={(event) => {
        request.current.invalidate(); setPicking(false); setError(null);
        onChange(event.target.checked ? { destination: "", port: "", identityFile: "" } : null);
      }} />
      <Cable size={15} aria-hidden="true" /><span>SSH jump host</span><small>Optional</small>
    </label>
    {value && <div className="ssh-fields-body">
      <div className="ssh-field">
        <label htmlFor={`${id}-host`}>SSH host or alias</label>
        <input id={`${id}-host`} value={value.destination} required maxLength={512} autoComplete="off" spellCheck={false}
          placeholder="reader@bastion or prod-bastion" onChange={(event) => onChange({ ...value, destination: event.target.value })} />
        <p className="ssh-help">Uses your OpenSSH configuration and agent. Your database address and sign-in method stay the same.</p>
      </div>
      <details className="ssh-options" open={advanced} onToggle={(event) => setAdvanced(event.currentTarget.open)}>
        <summary>Identity file &amp; port</summary>
        <div className="ssh-options-grid">
          <div className="ssh-field">
            <label htmlFor={`${id}-port`}>SSH port</label>
            <input id={`${id}-port`} type="text" inputMode="numeric" pattern="[0-9]+" value={value.port}
              placeholder="From SSH config" onChange={(event) => onChange({ ...value, port: event.target.value })} />
          </div>
          <div className="ssh-field">
            <label htmlFor={`${id}-key`}>Identity file</label>
            <div className="ssh-key-input">
              <input id={`${id}-key`} value={value.identityFile} maxLength={4096} autoComplete="off" spellCheck={false}
                placeholder="From SSH config or agent" onChange={(event) => onChange({ ...value, identityFile: event.target.value })} />
              {isDesktop() && <button type="button" className="btn ghost" aria-label="Choose SSH identity file" disabled={picking} onClick={() => void chooseKey()}><FolderOpen size={14} /></button>}
            </div>
          </div>
        </div>
        <p className="ssh-help">Encrypted keys need to be unlocked in your SSH agent. The bastion’s host key must already be trusted.</p>
      </details>
      {error && <p className="ssh-error" role="alert">{error}</p>}
    </div>}
  </fieldset>;
}
