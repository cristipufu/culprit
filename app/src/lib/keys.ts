/**
 * Shortcut labels are written the Mac way ("⌘⇧E"). On Windows and Linux they read "Ctrl+Shift+E"; the handlers accept
 * either modifier (lib/hotkeys.ts, CodeMirror's Mod-), so only the label changes.
 */
export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.userAgent);

const NAMES: Record<string, string> = { "⌘": "Ctrl+", "⇧": "Shift+", "⌥": "Alt+", "↵": "Enter" };

/** Any text with Mac shortcut symbols in it ("Press ⌘V to paste"), in this platform's words. */
export function keys(mac: string): string {
  return isMac ? mac : mac.replace(/[⌘⇧⌥↵]/g, (k) => NAMES[k] ?? k);
}
