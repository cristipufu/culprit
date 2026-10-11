import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";
import { App } from "./App";
import { isMac } from "./lib/keys";

// macOS draws its window buttons over the sidebar (titleBarStyle "Overlay"); Windows and Linux keep a native title bar.
document.documentElement.dataset.platform = isMac ? "mac" : "other";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
