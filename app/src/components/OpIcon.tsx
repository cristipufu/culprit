import {
  ArrowDownWideNarrow,
  Boxes,
  Calculator,
  Combine,
  CornerDownLeft,
  Funnel,
  GitMerge,
  Hash,
  Layers,
  ListOrdered,
  PencilLine,
  Repeat2,
  Rows3,
  ScanLine,
  ScanSearch,
  Shuffle,
  Sigma,
  Split,
  SquareFunction,
  Workflow,
  type LucideIcon,
} from "lucide-react";
import type { PlanNode } from "../gen/PlanNode";

/** The operator families people recognize at a glance: a seek, a scan, a lookup, a hash or loop join, a sort. */
export function opIcon(node: Pick<PlanNode, "op" | "kind">): LucideIcon {
  const op = node.op.toLowerCase();
  if (op.includes("lookup") && node.kind === "lookup") return CornerDownLeft;
  if (op.includes("nested loop")) return Repeat2;
  if (op.includes("hash")) return Hash;
  if (op.includes("merge join") || op === "merge") return GitMerge;
  if (op.includes("adaptive")) return Shuffle;
  if (op.startsWith("filter")) return Funnel;
  if (op === "top" || op === "limit" || op.startsWith("limit")) return ListOrdered;
  if (op.includes("constant") || op === "result" || op === "values scan") return Rows3;
  if (op === "query block" || op === "query" || op === "union") return Boxes;
  if (op.includes("compute scalar")) return SquareFunction;
  switch (node.kind) {
    case "seek":
      return ScanSearch;
    case "scan":
      return ScanLine;
    case "lookup":
      return CornerDownLeft;
    case "join":
      return Combine;
    case "sort":
      return ArrowDownWideNarrow;
    case "aggregate":
      return Sigma;
    case "spool":
      return Layers;
    case "parallelism":
      return Split;
    case "modify":
      return PencilLine;
    case "compute":
      return Calculator;
    default:
      return Workflow;
  }
}

export function OpIcon({ node, size = 14, color }: { node: Pick<PlanNode, "op" | "kind">; size?: number; color?: string }) {
  const Icon = opIcon(node);
  return <Icon size={size} strokeWidth={1.75} color={color} aria-hidden="true" />;
}
