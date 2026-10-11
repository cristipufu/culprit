import { useEffect, useRef } from "react";

type Handler = (event: KeyboardEvent) => void;

export type Bindings = Record<string, Handler>;

function isTyping(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  return target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "SELECT";
}

export function comboOf(event: KeyboardEvent): string {
  const parts: string[] = [];
  if (event.metaKey || event.ctrlKey) parts.push("mod");
  if (event.shiftKey && event.key.length > 1) parts.push("shift");
  if (event.altKey) parts.push("alt");
  parts.push(event.key.length === 1 ? event.key.toLowerCase() : event.key);
  return parts.join("+");
}

/** Global shortcuts. Plain keys are ignored while typing; `mod+` combos always fire. */
export function useHotkeys(bindings: Bindings, enabled = true): void {
  const ref = useRef(bindings);
  ref.current = bindings;

  useEffect(() => {
    if (!enabled) return;
    const onKey = (event: KeyboardEvent) => {
      const combo = comboOf(event);
      const handler = ref.current[combo];
      if (!handler) return;
      if (!combo.startsWith("mod+") && combo !== "Escape" && isTyping(event.target)) return;
      event.preventDefault();
      handler(event);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [enabled]);
}
