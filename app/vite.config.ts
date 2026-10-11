import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  // CodeMirror breaks when two copies of its core load (instanceof checks); keep one of each.
  resolve: { dedupe: ["@codemirror/state", "@codemirror/view", "@codemirror/language", "@codemirror/autocomplete"] },
  optimizeDeps: { include: ["codemirror", "@codemirror/state", "@codemirror/view", "@codemirror/lang-sql", "@codemirror/autocomplete"] },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
    fs: { allow: [".."] },
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    target: "safari16",
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    cssMinify: true,
  },
});
