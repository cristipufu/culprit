import { Bookmark, Pencil, Play, Search, Trash2 } from "lucide-react";
import { useMemo, useState } from "react";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { SavedQuery } from "../gen/SavedQuery";
import { api, errorMessage } from "../lib/api";
import { loadSaved, toast } from "../lib/store";

type Props = {
  current: ConnectionInfo;
  saved: SavedQuery[] | null;
  active: string | null;
  onOpen: (query: SavedQuery, run: boolean) => void;
  onRenamed: (from: string, to: string) => void;
  onDeleted: (name: string) => void;
};

function firstLine(sql: string): string {
  return (
    sql
      .split("\n")
      .map((l) => l.trim())
      .find((l) => l && !l.startsWith("--")) ?? ""
  );
}

/** The collection of saved queries: search, open, rerun, rename, delete. */
export function SavedRail({ current, saved, active, onOpen, onRenamed, onDeleted }: Props) {
  const [filter, setFilter] = useState("");
  const [renaming, setRenaming] = useState<{ from: string; to: string } | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const list = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return (saved ?? []).filter((s) => !q || s.name.toLowerCase().includes(q) || s.sql.toLowerCase().includes(q) || (s.connection ?? "").toLowerCase().includes(q));
  }, [saved, filter]);

  const rename = async () => {
    if (!renaming) return;
    const query = saved?.find((s) => s.name === renaming.from);
    const to = renaming.to.trim();
    if (!query || !to || to === query.name) return setRenaming(null);
    try {
      await (await api()).saveQuery({ name: to, sql: query.sql, connection: query.connection, description: query.description, previous_name: query.name });
      onRenamed(query.name, to);
      setRenaming(null);
      await loadSaved();
    } catch (e) {
      toast(errorMessage(e).message);
    }
  };

  const remove = async (name: string) => {
    try {
      await (await api()).deleteSavedQuery(name);
      onDeleted(name);
      setDeleting(null);
      await loadSaved();
      toast(`Deleted “${name}”`);
    } catch (e) {
      toast(errorMessage(e).message);
    }
  };

  return (
    <aside className="saved-rail">
      <div className="saved-head">
        <span className="saved-title">
          <Bookmark size={12} /> Saved queries
        </span>
        {saved && saved.length > 0 && <span className="muted">{saved.length}</span>}
      </div>
      {saved && saved.length > 4 && (
        <label className="search saved-search">
          <Search size={12} />
          <input value={filter} placeholder="Find a query" onChange={(e) => setFilter(e.target.value)} />
        </label>
      )}
      <div className="saved-list">
        {saved === null && <div className="muted saved-empty">Loading…</div>}
        {saved?.length === 0 && (
          <div className="saved-empty">
            Save the query in a tab with <span className="kbd">⌘S</span> to keep it here and rerun it in one click.
          </div>
        )}
        {list.map((q) => (
          <div key={q.name} className={`saved-item ${active === q.name ? "on" : ""}`} onClick={() => onOpen(q, false)} title={q.description ?? q.sql}>
            {renaming?.from === q.name ? (
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  void rename();
                }}
                onClick={(e) => e.stopPropagation()}
              >
                <input autoFocus className="saved-rename" value={renaming.to} onChange={(e) => setRenaming({ ...renaming, to: e.target.value })} onBlur={() => void rename()} onKeyDown={(e) => e.key === "Escape" && setRenaming(null)} />
              </form>
            ) : (
              <div className="saved-name">{q.name}</div>
            )}
            <div className="saved-sql">{firstLine(q.sql)}</div>
            <div className="saved-meta">
              {q.connection && <span className={`saved-conn ${q.connection === current.name ? "here" : ""}`}>{q.connection}</span>}
              <span className="spacer" />
              {deleting === q.name ? (
                <span className="saved-confirm" onClick={(e) => e.stopPropagation()}>
                  Delete?
                  <button className="link danger" onClick={() => remove(q.name)}>
                    Yes
                  </button>
                  <button className="link" onClick={() => setDeleting(null)}>
                    No
                  </button>
                </span>
              ) : (
                <span className="saved-actions" onClick={(e) => e.stopPropagation()}>
                  <button className="icon-btn" title={`Run on ${current.name}`} onClick={() => onOpen(q, true)}>
                    <Play size={12} />
                  </button>
                  <button className="icon-btn" title="Rename" onClick={() => setRenaming({ from: q.name, to: q.name })}>
                    <Pencil size={12} />
                  </button>
                  <button className="icon-btn" title="Delete" onClick={() => setDeleting(q.name)}>
                    <Trash2 size={12} />
                  </button>
                </span>
              )}
            </div>
          </div>
        ))}
      </div>
    </aside>
  );
}
