import { Camera } from "lucide-react";
import { canScreenshot, screenshotWorkspace } from "../lib/screenshot";

/** Top-bar button: the workspace as a PNG, without the sidebar, on the clipboard. */
export function ScreenshotButton() {
  if (!canScreenshot()) return null;
  return (
    <button
      className="btn ghost icon shot-btn"
      onClick={() => void screenshotWorkspace()}
      title="Screenshot this view: copies it without the sidebar"
      aria-label="Screenshot this view"
    >
      <Camera size={14} />
    </button>
  );
}
