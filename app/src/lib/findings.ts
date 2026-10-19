import type { Finding } from "../gen/Finding";

/** Old saved reports have no classification until reanalysis. Their severity remains the fallback. */
export function actionability(f: Finding): Finding["actionability"] {
  return f.inactive ? "history" : (f.actionability ?? (f.severity === "low" ? "optional-improvement" : "needs-attention"));
}
export function isActionable(f: Finding): boolean {
  return !f.inactive && ["needs-attention", "optional-improvement"].includes(actionability(f));
}
export function actionLabel(f: Finding): string {
  if (f.inactive) return "Stopped running";
  return { "needs-attention": "Needs attention", "optional-improvement": "Optional improvement", information: "Information", history: "History" }[actionability(f)];
}
