import type { LiveState } from "../gen/LiveState";
import type { Session } from "../gen/Session";
import type { Tile } from "../gen/Tile";
import type { TileItem } from "../gen/TileItem";
import type { TileKey } from "../gen/TileKey";
import type { TileStatus } from "../gen/TileStatus";
import { duration } from "./format";
import { querySummary } from "./query-summary";

/**
 * The "right now" tile from a live read. The app polls sessions itself, so this mirrors `culprit_checks::dashboard::now`:
 * the same status, takeaway and numbers (the CLI and MCP server build that tile from one read).
 */
function blockingStatus(waiters: number, ms: number | null): TileStatus {
  return (ms ?? 0) >= 30_000 ? "bad" : waiters >= 5 || (ms ?? 0) >= 1000 ? "watch" : "info";
}

export function nowTile(live: LiveState): Tile {
  const running = (s: Session) => s.state !== "idle" && s.state !== "sleeping" && !s.state.startsWith("idle in transaction");
  const idleInTransaction = (s: Session) => s.transaction_ms != null && (s.state.startsWith("idle in transaction") || s.state === "sleeping");
  const active = live.sessions.filter(running);
  const blocked = live.sessions.filter((s) => s.blocked_by).length;
  const idleTx = live.sessions.filter(idleInTransaction);
  const longest = active.filter((s) => !s.blocked_by).reduce<Session | null>((a, s) => ((s.elapsed_ms ?? 0) > (a?.elapsed_ms ?? 0) || !a ? s : a), null);
  const oldestIdle = idleTx.reduce<Session | null>((a, s) => ((s.transaction_ms ?? 0) > (a?.transaction_ms ?? 0) || !a ? s : a), null);
  const chains = live.blocking?.chains ?? [];
  const incomplete = !!live.notes?.length || live.sessions.some((s) => !s.blocked_by && !!s.wait && (s.wait.toLowerCase().includes("lock") || s.wait.startsWith("LCK_")));

  let status: TileStatus;
  let takeaway: string;
  const chain = chains[0];
  if (chain) {
    status = blockingStatus(chain.waiters, chain.longest_wait_ms);
    takeaway = chain.headline;
  } else if (blocked > 0) {
    const waits = live.sessions.filter((s) => s.blocked_by && s.wait_ms != null).map((s) => s.wait_ms!);
    status = blockingStatus(blocked, waits.length ? Math.max(...waits) : null);
    takeaway = `${capitalized(plural(blocked, "session is", "sessions are"))} waiting on another session's lock.`;
  } else if (oldestIdle && (oldestIdle.transaction_ms ?? 0) >= 600_000) {
    status = "watch";
    takeaway = `Session ${oldestIdle.id} is idle with a transaction opened ${duration(oldestIdle.transaction_ms ?? 0)} ago.`;
  } else if (incomplete) {
    status = "info";
    takeaway = "No blocking chain was established; lock visibility is incomplete.";
  } else if (active.length === 0) {
    status = "good";
    takeaway = "Nothing is running right now.";
  } else {
    const ms = longest?.elapsed_ms ?? 0;
    const tail = ms >= 1000 ? `; the longest has run ${duration(ms)}` : "";
    status = "good";
    takeaway = `${capitalized(plural(active.length, "session", "sessions"))} running, none blocked${tail}.`;
  }
  const item = (id: string, label: string, value: string | null, itemStatus: TileStatus): TileItem => ({
    kind: "session",
    id,
    label,
    value,
    share: null,
    status: itemStatus,
    query_id: null,
    finding_id: null,
    at: null,
  });
  const items = chains.slice(0, 3).map((c) => item(c.head, c.headline, `${c.waiters} waiting`, blockingStatus(c.waiters, c.longest_wait_ms)));
  [...active]
    .sort((a, b) => (b.elapsed_ms ?? 0) - (a.elapsed_ms ?? 0))
    .filter((s) => !chains.some((c) => c.head === s.id))
    .slice(0, 3)
    .forEach((s) => {
      const what = s.sql ? sessionLabel(s.sql) : (s.command ?? s.state);
      items.push(item(s.id, `Session ${s.id} · ${what}`, s.elapsed_ms != null ? duration(s.elapsed_ms) : null, s.blocked_by ? blockingStatus(1, s.wait_ms) : "good"));
    });
  const metric = (key: string, label: string, value: number) => ({ key, label, value, text: String(value) });
  return {
    key: "now",
    title: "Right now",
    status,
    takeaway,
    detail: live.notes?.join(" ") || null,
    metrics: [
      metric("active", "Running", active.length),
      { key: "blocked", label: "Identified blocked sessions", value: incomplete ? null : blocked, text: incomplete ? `at least ${blocked}` : String(blocked) },
      metric("idle-in-transaction", "Idle in transaction", idleTx.length),
      metric("long-idle-in-transaction", "Long open idle transactions", idleTx.filter((s) => (s.transaction_ms ?? 0) >= 600_000).length),
      metric("sessions", "Sessions", live.sessions.length),
    ],
    items,
    fix_sql: null,
    next: null,
  };
}

/** The tile when sessions couldn't be read; the rest of the dashboard stands. */
export function nowUnavailable(reason: string): Tile {
  return {
    key: "now",
    title: "Right now",
    status: "unavailable",
    takeaway: `Couldn't read sessions: ${reason.replace(/\.$/, "")}.`,
    detail: "Everything else here comes from the latest scan.",
    metrics: [],
    items: [],
    fix_sql: null,
    next: null,
  };
}

function sessionLabel(sql: string): string {
  const s = querySummary(sql);
  return s.target ? `${s.verb} ${s.target}` : s.verb;
}

function plural(n: number, one: string, many: string): string {
  return n === 1 ? `1 ${one}` : `${n} ${many}`;
}

function capitalized(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** Most urgent first when two tiles share a status (mirrors `PRIORITY` in Rust). */
const PRIORITY: TileKey[] = [
  "now",
  "db-time",
  "findings",
  "statistics",
  "failures",
  "replication",
  "deadlocks",
  "storage",
  "health",
  "connections",
  "waits",
  "plans",
  "indexes",
  "top-queries",
];
const SEVERITY: Record<TileStatus, number> = { bad: 4, watch: 3, unavailable: 2, info: 1, good: 0 };

/** Tiles that need a look, most urgent first, and the sentence that leads the page. */
export function attention(tiles: Tile[]): { keys: TileKey[]; headline: string } {
  // Without query statistics, turning them on comes first: only a problem happening right now is more urgent (mirrors Rust).
  const noWorkload = tiles.some((t) => t.key === "db-time" && t.status === "unavailable");
  const rank = (k: TileKey) => (k === "now" ? 0 : k === "statistics" && noWorkload ? 1 : PRIORITY.indexOf(k) + 1);
  const urgent = tiles
    .filter((t) => t.status === "bad" || t.status === "watch")
    .sort((a, b) => SEVERITY[b.status] - SEVERITY[a.status] || rank(a.key) - rank(b.key));
  if (urgent.length > 0) return { keys: urgent.map((t) => t.key), headline: urgent[0]!.takeaway };
  const missing = tiles.filter((t) => t.status === "unavailable").length;
  const headline =
    missing === 0
      ? "Nothing needs a look right now."
      : missing === 1
        ? "Nothing needs a look in what this scan can see; one tile has no data here."
        : `Nothing needs a look in what this scan can see; ${missing} tiles have no data here.`;
  return { keys: [], headline };
}

export const STATUS_LABEL: Record<TileStatus, string> = {
  bad: "Needs a look now",
  watch: "Worth a look",
  good: "Fine",
  info: "For context",
  unavailable: "Not available here",
};
