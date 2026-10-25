import type { ConnectionInfo } from "../gen/ConnectionInfo";

export type EditorTab = { id: string; title: string; sql: string; saved: string | null };
export type EditorTabs = { tabs: EditorTab[]; active: string };
type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;
type Connection = Pick<ConnectionInfo, "name" | "engine" | "host" | "port" | "database">;
type Index = { version: 1; tabs: Omit<EditorTab, "sql">[]; active: string };

export const MAX_EDITOR_TABS = 200;
const MAX_SQL_CHARS = 2 * 1024 * 1024;
const MAX_INDEX_CHARS = 64 * 1024;
const LEGACY_KEY = "culprit.editor.tabs";
const BODY_PREFIX = "culprit.editor.sql:";
const UNAVAILABLE = "Scratch tabs couldn’t be saved on this device. Save important work as named queries.";
const INVALID = "Saved scratch tabs couldn’t be read. Their stored copy has been kept.";
const TOO_LARGE = "These scratch tabs exceed the device storage limit. Save important work as named queries.";

export function editorScope(connection: Connection): string {
  return `culprit.editor.workspace:${encodeURIComponent(JSON.stringify([connection.name, connection.engine, connection.host, connection.port, connection.database]))}`;
}

export function newEditorTab(sql = "", title = "Query 1", saved: string | null = null): EditorTab {
  const id = Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) => byte.toString(16).padStart(2, "0")).join("");
  return { id, sql, title, saved };
}

function deviceStorage(): StorageLike | null {
  try { return globalThis.localStorage; } catch { return null; }
}

function metadata(tabs: EditorTabs): string {
  return JSON.stringify({ version: 1, active: tabs.active, tabs: tabs.tabs.map(({ id, title, saved }) => ({ id, title, saved })) } satisfies Index);
}

function validate(value: unknown, legacy = false): EditorTabs {
  if (!value || typeof value !== "object") throw new Error(INVALID);
  const index = value as Partial<Index & EditorTabs>;
  if ((!legacy && index.version !== 1) || !Array.isArray(index.tabs) || !index.tabs.length || index.tabs.length > MAX_EDITOR_TABS) throw new Error(INVALID);
  const ids = new Set<string>();
  for (const tab of index.tabs) {
    if (!tab || typeof tab.id !== "string" || !/^[\w-]{1,64}$/.test(tab.id) || ids.has(tab.id)
      || typeof tab.title !== "string" || tab.title.length > 1024 || (tab.saved !== null && typeof tab.saved !== "string")
      || (typeof tab.saved === "string" && tab.saved.length > 1024) || (legacy && typeof tab.sql !== "string")) throw new Error(INVALID);
    ids.add(tab.id);
  }
  return { tabs: index.tabs as EditorTab[], active: typeof index.active === "string" && ids.has(index.active) ? index.active : index.tabs[0]!.id };
}

function load(raw: string, storage: StorageLike): EditorTabs {
  if (raw.length > MAX_INDEX_CHARS) throw new Error(INVALID);
  const parsed = validate(JSON.parse(raw));
  let chars = 0;
  return { ...parsed, tabs: parsed.tabs.map(({ id, title, saved }) => {
    const sql = storage.getItem(BODY_PREFIX + id);
    if (sql === null || (chars += sql.length) > MAX_SQL_CHARS) throw new Error(INVALID);
    return { id, title, saved, sql };
  }) };
}

/** Each tab owns a random body key. Keystrokes write that body alone; the small index holds order and selection. */
export class EditorTabStorage {
  readonly initial: EditorTabs;
  error: string | null = null;
  private readonly scope: string;
  private index: string | null = null;
  private bodies = new Map<string, string>();
  private legacy = false;
  private blocked = false;

  constructor(connection: Connection, placeholder: string, private readonly storage = deviceStorage()) {
    this.scope = editorScope(connection);
    const fresh = newEditorTab(placeholder);
    this.initial = { tabs: [fresh], active: fresh.id };
    if (!storage) { this.error = UNAVAILABLE; return; }
    try {
      const raw = storage.getItem(this.scope);
      if (raw !== null) {
        this.initial = load(raw, storage);
        this.index = metadata(this.initial);
        this.bodies = new Map(this.initial.tabs.map((t) => [t.id, t.sql]));
      } else {
        const legacy = storage.getItem(LEGACY_KEY);
        if (legacy !== null) {
          if (legacy.length > MAX_SQL_CHARS + MAX_INDEX_CHARS) throw new Error(INVALID);
          const old = validate(JSON.parse(legacy), true);
          if (old.tabs.reduce((sum, t) => sum + t.sql.length, 0) > MAX_SQL_CHARS) throw new Error(INVALID);
          const tabs = old.tabs.map((t) => newEditorTab(t.sql, t.title, t.saved));
          this.initial = { tabs, active: tabs[old.tabs.findIndex((t) => t.id === old.active)]!.id };
          this.legacy = true;
        }
      }
    } catch {
      this.error = INVALID;
      this.blocked = true;
    }
  }

  /** Claim old global tabs only after mounting and after a successful copy, never during React render. */
  migrate(): string | null {
    return this.legacy ? this.write(this.initial) : this.error;
  }

  write(tabs: EditorTabs): string | null {
    if (this.blocked) return this.error;
    if (!this.storage) return this.error = UNAVAILABLE;
    const index = metadata(tabs);
    if (tabs.tabs.length > MAX_EDITOR_TABS || index.length > MAX_INDEX_CHARS || tabs.tabs.reduce((sum, t) => sum + t.sql.length, 0) > MAX_SQL_CHARS) return this.error = TOO_LARGE;
    const added: string[] = [];
    let committed = false;
    try {
      for (const tab of tabs.tabs) {
        if (this.bodies.get(tab.id) !== tab.sql) {
          this.storage.setItem(BODY_PREFIX + tab.id, tab.sql);
          if (!this.bodies.has(tab.id)) added.push(tab.id);
          this.bodies.set(tab.id, tab.sql);
        }
      }
      if (this.index !== index) {
        this.storage.setItem(this.scope, index);
        this.index = index;
      }
      committed = true;
      // Retire bodies only after the index no longer references them.
      const kept = new Set(tabs.tabs.map((t) => t.id));
      for (const id of this.bodies.keys()) {
        if (!kept.has(id)) { this.storage.removeItem(BODY_PREFIX + id); this.bodies.delete(id); }
      }
      if (this.legacy) { this.storage.removeItem(LEGACY_KEY); this.legacy = false; }
      return this.error = null;
    } catch {
      // A rejected index must not leave newly allocated SQL bodies consuming quota forever.
      if (!committed) for (const id of added) {
        try { this.storage.removeItem(BODY_PREFIX + id); this.bodies.delete(id); } catch { /* Keep the source index intact. */ }
      }
      return this.error = UNAVAILABLE;
    }
  }
}

/** Bodies keep their IDs, so renaming never duplicates megabytes of SQL or runs out of quota copying it. */
export function renameEditorTabs(from: Connection, to: Connection, storage = deviceStorage()): string | null {
  if (!storage || editorScope(from) === editorScope(to)) return null;
  try {
    const raw = storage.getItem(editorScope(from));
    if (raw !== null) {
      load(raw, storage);
      storage.setItem(editorScope(to), raw);
      storage.removeItem(editorScope(from));
    }
    return null;
  } catch { return "The connection was renamed, but its scratch tabs couldn’t be moved. Their stored copy has been kept."; }
}
