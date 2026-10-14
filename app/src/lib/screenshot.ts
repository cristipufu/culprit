import { api, errorMessage, isDesktop } from "./api";
import { setState, toast } from "./store";

/** The webview takes the picture itself, which the desktop app does on macOS only so far. */
export function canScreenshot(): boolean {
  return isDesktop() && navigator.userAgent.includes("Mac");
}

const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

let taking = false;

/**
 * A PNG of the workspace (the top bar and the view, never the sidebar with its connection names), exactly as the
 * window shows it, on the clipboard. The button and any toast are hidden for that frame.
 */
export async function screenshotWorkspace(): Promise<void> {
  const pane = document.querySelector<HTMLElement>("main.main");
  if (!pane || taking) return;
  taking = true;
  setState({ toast: null, palette: false });
  document.documentElement.classList.add("capturing");
  try {
    await frame();
    await frame();
    const r = pane.getBoundingClientRect();
    await (await api()).screenshot({ x: r.left, y: r.top, width: r.width, height: r.height });
    toast("Screenshot copied");
  } catch (e) {
    toast(errorMessage(e).message);
  } finally {
    document.documentElement.classList.remove("capturing");
    taking = false;
  }
}
