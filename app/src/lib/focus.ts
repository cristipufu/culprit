import { useEffect, useRef, type RefObject } from "react";

const FOCUSABLE =
  'a[href], button, input, select, textarea, summary, [tabindex]:not([tabindex="-1"])';
const traps: HTMLElement[] = [];

/** Keep focus and keyboard events in the topmost modal, including when focused controls disappear or become disabled. */
export function useFocusTrap(ref: RefObject<HTMLElement | null>, active = true, onEscape?: () => void): void {
  const escape = useRef(onEscape); escape.current = onEscape;
  useEffect(() => {
    const root = ref.current;
    if (!active || !root) return;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const addedTabIndex = !root.hasAttribute("tabindex");
    if (addedTabIndex) root.tabIndex = -1;
    traps.push(root);
    const top = () => traps.at(-1) === root;
    const items = () => [...root.querySelectorAll<HTMLElement>(FOCUSABLE)]
      .filter((el) => !el.matches(":disabled") && el.tabIndex >= 0 && el.getClientRects().length > 0);
    const contain = () => {
      if (top() && root.isConnected && !root.contains(document.activeElement)) root.focus({ preventScroll: true });
    };
    const onKey = (event: KeyboardEvent) => {
      if (!top()) return;
      const inside = event.target instanceof Node && root.contains(event.target);
      if (!inside) {
        contain();
        event.stopPropagation();
        if (event.key === "Escape" && escape.current) { event.preventDefault(); escape.current(); }
      }
      if (event.key !== "Tab") return;
      const controls = items();
      const first = controls[0], last = controls.at(-1);
      if (!first || !last) { event.preventDefault(); root.focus(); return; }
      const current = document.activeElement;
      if (event.shiftKey && (current === first || current === root || !inside)) {
        event.preventDefault(); last.focus();
      } else if (!event.shiftKey && (current === last || current === root || !inside)) {
        event.preventDefault(); first.focus();
      }
    };
    const onBlur = () => queueMicrotask(contain);
    const observer = new MutationObserver(contain);
    observer.observe(root, { subtree: true, childList: true, attributes: true, attributeFilter: ["disabled", "hidden"] });
    document.addEventListener("focusin", contain, true);
    document.addEventListener("focusout", onBlur, true);
    document.addEventListener("keydown", onKey, true);
    return () => {
      const wasTop = top();
      traps.splice(traps.indexOf(root), 1);
      observer.disconnect();
      document.removeEventListener("focusin", contain, true);
      document.removeEventListener("focusout", onBlur, true);
      document.removeEventListener("keydown", onKey, true);
      if (addedTabIndex) root.removeAttribute("tabindex");
      if (wasTop && previous && document.contains(previous)) previous.focus();
    };
  }, [ref, active]);
}
