import { Check, Database, FolderOpen } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import type { Engine } from "../../gen/Engine";
import type { Environment } from "../../gen/Environment";
import type { ParsedConnection } from "../../gen/ParsedConnection";
import { api, errorMessage, type ApiError } from "../../lib/api";
import { connectFacts, ENGINES } from "../../lib/engines";
import { LatestRequest } from "../../lib/requests";
import { loadConnections, openConnection } from "../../lib/store";
import { remember, remembered } from "./DiscoverPanel";
import { EnginePicker } from "./EnginePicker";

const ENVIRONMENTS: Environment[] = ["production", "staging", "development", "local"];

type TestState = { status: "idle" } | { status: "busy" } | { status: "ok"; label: string } | { status: "error"; error: ApiError };

/** The engine picked last time, picked again on the next new connection. */
const ENGINE_KEY = "culprit.new.engine";

/** A guess from the text itself: a name that says production or staging, a local host, a file on this computer. */
function guessEnvironment(text: string, engine: Engine): Environment {
  if (/prod/i.test(text)) return "production";
  if (/alpha|staging|stg/i.test(text)) return "staging";
  if (/localhost|127\.0\.0\.1/i.test(text) || connectFacts(engine).file) return "local";
  return "development";
}

/**
 * The engine first, then what it needs: a connection string for a server, the file for SQLite. The pick settles what
 * the text leaves open (a bare host, an ADO.NET string); a text that names another engine (a postgres:// URL, a .db
 * path) switches the pick and says so. Before anything is picked, a pasted connection string picks its own engine.
 */
export function NewConnectionForm({ onDone, canCancel }: { onDone: () => void; canCancel: boolean }) {
  const [engine, setEngine] = useState<Engine | null>(() => {
    const last = remembered(ENGINE_KEY) as Engine;
    return ENGINES.includes(last) ? last : null;
  });
  const [switched, setSwitched] = useState(false);
  const [input, setInput] = useState("");
  const [parsed, setParsed] = useState<ParsedConnection | null>(null);
  const parsedInput = useRef("");
  const [parseError, setParseError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [environment, setEnvironment] = useState<Environment>("development");
  const [environmentTouched, setEnvironmentTouched] = useState(false);
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const [test, setTest] = useState<TestState>({ status: "idle" });
  const field = useRef<HTMLInputElement>(null);
  const focusInput = useRef(engine !== null);
  const ids = useId();
  const facts = engine ? connectFacts(engine) : null;
  const action = useRef(new LatestRequest());

  useEffect(() => {
    action.current.invalidate();
    setTest({ status: "idle" });
    return () => action.current.invalidate();
  }, [input, engine, name, environment, user, password]);

  useEffect(() => {
    if (!input.trim()) {
      setParsed(null);
      setParseError(null);
      return;
    }
    let live = true;
    const handle = setTimeout(async () => {
      try {
        const p = await (await api()).parseConnection(input, engine);
        if (!live) return;
        // The text names another engine than the one picked (or none was): it's that engine's, read again as it.
        if (p.engine && p.engine !== engine) {
          focusInput.current = true;
          setSwitched(engine !== null);
          setEngine(p.engine);
          return;
        }
        parsedInput.current = input;
        setParsed(p);
        setParseError(null);
        if (!nameTouched) setName(p.suggested_name);
        if (!environmentTouched && p.engine) setEnvironment(guessEnvironment(input, p.engine));
      } catch (e) {
        if (!live) return;
        setParsed(null);
        setParseError(errorMessage(e).message);
      }
    }, 150);
    return () => {
      live = false;
      clearTimeout(handle);
    };
  }, [input, engine, nameTouched, environmentTouched]);

  // Restore the input on opening or after paste detection. Manual selection keeps focus in the picker for Tab.
  useEffect(() => {
    if (engine && focusInput.current) {
      field.current?.focus();
      focusInput.current = false;
    }
  }, [engine]);

  const pick = (next: Engine) => {
    if (next === engine) return;
    // A path and a connection string are different things: switching between a file and a server starts over.
    if (engine && connectFacts(engine).file !== connectFacts(next).file) setInput("");
    setEngine(next);
    setSwitched(false);
    setTest({ status: "idle" });
  };

  // Before an engine is picked, a pasted connection string picks its own.
  const onPaste = (e: React.ClipboardEvent) => {
    if (engine || e.target === field.current) return;
    const text = e.clipboardData.getData("text").trim();
    if (!text) return;
    e.preventDefault();
    setInput(text);
  };

  const onChoose = async () => {
    const current = action.current.start();
    try {
      const chosen = await (await api()).chooseDatabaseFile();
      if (current() && chosen) setInput(chosen);
    } catch (err) {
      if (current()) setParseError(errorMessage(err).message);
    }
  };

  // What was read is the picked engine's: the details and the buttons follow.
  const ready = !!parsed && !!engine && parsed.engine === engine && parsedInput.current === input;
  const needsUser = ready && parsed.auth === "password" && !parsed.user;
  const needsPassword = ready && parsed.auth === "password" && !parsed.has_password;

  const request = () => ({
    name,
    input,
    environment,
    engine,
    database: null,
    user: needsUser && user ? user : null,
    password: needsPassword && password ? password : null,
    password_env: null,
    auth: null,
  });

  const onTest = async () => {
    const current = action.current.start();
    setTest({ status: "busy" });
    try {
      const server = await (await api()).testConnection(request());
      if (current()) setTest({ status: "ok", label: `${server.product} · ${server.database}` });
    } catch (e) {
      if (current()) setTest({ status: "error", error: errorMessage(e) });
    }
  };

  const onSave = async () => {
    const current = action.current.start();
    const candidate = request();
    setTest({ status: "busy" });
    try {
      const client = await api();
      if (!current()) return;
      await client.testConnection(candidate);
      if (!current()) return;
      const saved = await client.addConnection(candidate);
      if (!current()) return;
      if (engine) remember(ENGINE_KEY, engine);
      await loadConnections();
      if (!current()) return;
      onDone();
      openConnection(saved.name);
    } catch (e) {
      if (current()) setTest({ status: "error", error: errorMessage(e) });
    }
  };

  return (
    <form
      className="new-conn"
      onPaste={onPaste}
      onSubmit={(e) => {
        e.preventDefault();
        if (ready && name && test.status !== "busy") onSave();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape" && canCancel) onDone();
      }}
    >
      <div className="eyebrow">New connection</div>
      <div className="new-conn-connect">
        <div className="new-conn-field">
          <label htmlFor={`${ids}-engine`}>Database type</label>
          <EnginePicker id={`${ids}-engine`} value={engine} onChange={pick} autoFocus={!engine} />
        </div>
        <div className="new-conn-field">
          <label htmlFor={`${ids}-input`}>{facts?.file ? "Database file" : "Connection string"}</label>
          <div className="smart-input">
            <Database size={16} color="var(--text-3)" aria-hidden />
            <input ref={field} id={`${ids}-input`} value={input} spellCheck={false}
              autoComplete="off" autoCapitalize="off" placeholder={facts?.example ?? "Paste a connection string…"}
              aria-describedby={`${ids}-help`}
              onChange={(event) => { setInput(event.target.value); setSwitched(false); setTest({ status: "idle" }); }} />
            {facts?.file && (
              <button type="button" className={`btn ${input.trim() ? "" : "primary"}`} onClick={onChoose}>
                <FolderOpen size={13} /> Choose file…
              </button>
            )}
          </div>
        </div>
      </div>
      <p id={`${ids}-help`} className={`new-conn-help ${parseError ? "error" : ""}`}>
        {parseError ? parseError : ready && !facts?.file ? (
          <>
            <Check size={12} className="read" aria-hidden />
            {switched && <b>Switched to {facts?.choice}, which the text names. </b>}
            <span className="mono">{parsed.display}</span>
            {parsed.auth === "entra" && " · Microsoft Entra ID through your az login, no password kept"}
          </>
        ) : (
          <>
            {switched && <b>Switched to {facts?.choice}, which the text names. </b>}
            {facts?.accepts ?? "Paste a connection string to detect the database, or select a type."}
          </>
        )}
      </p>

      {ready && (
        <div className="form-grid">
          <label htmlFor={`${ids}-name`}>Name</label>
          <input
            id={`${ids}-name`}
            value={name}
            spellCheck={false}
            onChange={(e) => {
              setName(e.target.value);
              setNameTouched(true);
            }}
          />
          <label>Environment</label>
          <div className="seg" role="radiogroup" aria-label="Environment">
            {ENVIRONMENTS.map((env) => (
              <button
                key={env}
                type="button"
                role="radio"
                aria-checked={environment === env}
                className={environment === env ? "on" : ""}
                onClick={() => {
                  setEnvironment(env);
                  setEnvironmentTouched(true);
                }}
              >
                <span className={`env-dot env-${env}`} style={{ display: "inline-block", marginRight: 6, width: 6, height: 6, boxShadow: "none" }} />
                {env}
              </button>
            ))}
          </div>
          {needsUser && (
            <>
              <label htmlFor={`${ids}-user`}>User</label>
              <input id={`${ids}-user`} value={user} spellCheck={false} autoComplete="off" onChange={(e) => setUser(e.target.value)} placeholder="Not in the connection string" />
            </>
          )}
          {needsPassword && (
            <>
              <label htmlFor={`${ids}-password`}>Password</label>
              <input id={`${ids}-password`} type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder="Kept in the OS keychain" />
            </>
          )}
          <span />
          <div className="new-conn-actions">
            <button type="submit" className="btn primary" disabled={!name || test.status === "busy"}>
              Connect and scan
            </button>
            <button type="button" className="btn" disabled={test.status === "busy"} onClick={onTest}>
              Test
            </button>
            {canCancel && (
              <button type="button" className="btn ghost" onClick={onDone}>
                Cancel
              </button>
            )}
            {test.status === "busy" && <span className="spinner" />}
            {test.status === "ok" && <span className="tag good">✓ {test.label}</span>}
          </div>
          {test.status === "error" && (
            <>
              <span />
              <div className="notice error">
                <div>
                  <b>{test.error.message}</b>
                  {test.error.hint && <div>{test.error.hint}</div>}
                </div>
              </div>
            </>
          )}
          {environment === "production" && (
            <>
              <span />
              <div className="notice">Production connections are read-only in Culprit: scans read statistics views, plans are estimated, and anything that executes a statement asks first.</div>
            </>
          )}
        </div>
      )}
    </form>
  );
}
