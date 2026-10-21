import { Check, Database, FolderOpen } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { EngineBadge } from "../../components/EngineBadge";
import type { Engine } from "../../gen/Engine";
import type { Environment } from "../../gen/Environment";
import type { ParsedConnection } from "../../gen/ParsedConnection";
import { api, errorMessage, type ApiError } from "../../lib/api";
import { connectFacts, ENGINES } from "../../lib/engines";
import { loadConnections, openConnection } from "../../lib/store";
import { remember, remembered } from "./DiscoverPanel";

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
  const [parseError, setParseError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [environment, setEnvironment] = useState<Environment>("development");
  const [environmentTouched, setEnvironmentTouched] = useState(false);
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const [test, setTest] = useState<TestState>({ status: "idle" });
  const field = useRef<HTMLInputElement>(null);
  const ids = useId();
  const facts = engine ? connectFacts(engine) : null;

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
          setSwitched(engine !== null);
          setEngine(p.engine);
          return;
        }
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

  // Picking an engine puts the cursor where its connection string (or file) goes.
  useEffect(() => {
    if (engine) field.current?.focus();
  }, [engine]);

  const pick = (next: Engine) => {
    if (next === engine) return;
    // A path and a connection string are different things: switching between a file and a server starts over.
    if (engine && connectFacts(engine).file !== connectFacts(next).file) setInput("");
    setEngine(next);
    setSwitched(false);
    setTest({ status: "idle" });
  };

  const onPickerKey = (e: React.KeyboardEvent) => {
    const step = e.key === "ArrowRight" || e.key === "ArrowDown" ? 1 : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const at = engine ? ENGINES.indexOf(engine) : step > 0 ? -1 : 0;
    const next = ENGINES[(at + step + ENGINES.length) % ENGINES.length];
    if (!next) return;
    pick(next);
    (e.currentTarget.querySelector(`[data-engine="${next}"]`) as HTMLElement | null)?.focus();
  };

  // Before an engine is picked, a pasted connection string picks its own.
  const onPaste = (e: React.ClipboardEvent) => {
    if (engine) return;
    const text = e.clipboardData.getData("text").trim();
    if (!text) return;
    e.preventDefault();
    setInput(text);
  };

  const onChoose = async () => {
    try {
      const chosen = await (await api()).chooseDatabaseFile();
      if (chosen) setInput(chosen);
    } catch (err) {
      setParseError(errorMessage(err).message);
    }
  };

  // What was read is the picked engine's: the details and the buttons follow.
  const ready = !!parsed && !!engine && parsed.engine === engine;
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
    setTest({ status: "busy" });
    try {
      const server = await (await api()).testConnection(request());
      setTest({ status: "ok", label: `${server.product} · ${server.database}` });
    } catch (e) {
      setTest({ status: "error", error: errorMessage(e) });
    }
  };

  const onSave = async () => {
    setTest({ status: "busy" });
    try {
      const client = await api();
      await client.testConnection(request());
      const saved = await client.addConnection(request());
      if (engine) remember(ENGINE_KEY, engine);
      await loadConnections();
      onDone();
      openConnection(saved.name);
    } catch (e) {
      setTest({ status: "error", error: errorMessage(e) });
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
      <div className="engine-picker" role="radiogroup" aria-label="Database engine" onKeyDown={onPickerKey}>
        {ENGINES.map((e) => (
          <button
            key={e}
            type="button"
            role="radio"
            data-engine={e}
            aria-checked={engine === e}
            tabIndex={(engine ?? ENGINES[0]) === e ? 0 : -1}
            autoFocus={!engine && e === ENGINES[0]}
            className={`engine-choice ${engine === e ? "on" : ""}`}
            onClick={() => pick(e)}
          >
            <EngineBadge engine={e} size="md" />
            <span>{connectFacts(e).choice}</span>
          </button>
        ))}
      </div>

      {!engine && (
        <p className="new-conn-help">
          {input.trim() && !parseError
            ? `Which engine is ${input.trim().length > 60 ? `${input.trim().slice(0, 60)}…` : input.trim()}? Pick it above.`
            : parseError ?? "Pick the engine, or paste a connection string and Culprit picks it."}
        </p>
      )}

      {engine && facts && (
        <div className="new-conn-field">
          <label htmlFor={`${ids}-input`}>{facts.file ? "Database file" : "Connection string"}</label>
          <div className="smart-input">
            <Database size={16} color="var(--text-3)" aria-hidden />
            <input
              ref={field}
              id={`${ids}-input`}
              value={input}
              spellCheck={false}
              autoComplete="off"
              autoCapitalize="off"
              placeholder={facts.example}
              aria-describedby={`${ids}-help`}
              onChange={(e) => {
                setInput(e.target.value);
                setSwitched(false);
              }}
            />
            {/* A file is chosen in the system's own dialog rather than typed. */}
            {facts.file && (
              <button type="button" className={`btn ${input.trim() ? "" : "primary"}`} onClick={onChoose}>
                <FolderOpen size={13} /> Choose file…
              </button>
            )}
          </div>
          <p id={`${ids}-help`} className={`new-conn-help ${parseError ? "error" : ""}`}>
            {parseError ? (
              parseError
            ) : ready && !facts.file ? (
              <>
                <Check size={12} className="read" aria-hidden />
                {switched && <b>Switched to {facts.choice}, which the text names. </b>}
                <span className="mono">{parsed.display}</span>
                {parsed.auth === "entra" && " · Microsoft Entra ID through your az login, no password kept"}
              </>
            ) : (
              <>
                {switched && <b>Switched to {facts.choice}, which the text names. </b>}
                {facts.accepts}
              </>
            )}
          </p>
        </div>
      )}

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
