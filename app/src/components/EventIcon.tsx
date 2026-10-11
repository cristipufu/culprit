import { ChartColumn, CircleArrowUp, CloudAlert, CodeXml, Eraser, Flag, ListTree, Power, SlidersHorizontal, Table2 } from "lucide-react";
import type { EventKind } from "../gen/EventKind";

const ICONS = {
  mark: Flag,
  index: ListTree,
  table: Table2,
  code: CodeXml,
  setting: SlidersHorizontal,
  version: CircleArrowUp,
  restart: Power,
  "stats-reset": Eraser,
  statistics: ChartColumn,
  throttle: CloudAlert,
} satisfies Record<EventKind, unknown>;

/** The icon for a kind of change, the same on charts, in the timeline and in What changed. */
export function EventIcon({ kind, size = 12 }: { kind: EventKind; size?: number }) {
  const Icon = ICONS[kind];
  return <Icon size={size} aria-hidden="true" />;
}

/** Color family: marks are the person's own, throttling is the cloud's, the rest are the database's. */
export function eventTone(kind: EventKind): "mark" | "throttle" | "minor" | "change" {
  if (kind === "mark") return "mark";
  if (kind === "throttle") return "throttle";
  if (kind === "statistics") return "minor";
  return "change";
}
