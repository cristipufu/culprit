import type { ExportFile } from "../gen/ExportFile";
import type { ExportOptions } from "../gen/ExportOptions";
import { api, copyText, errorMessage } from "./api";
import { toast } from "./store";

const REDACT_KEY = "culprit.share.redact";

/** Redacting literal values is on unless the person turned it off (remembered per browser). */
export function rememberedRedact(): boolean {
  try {
    return localStorage.getItem(REDACT_KEY) !== "off";
  } catch {
    return true;
  }
}

export function rememberRedact(on: boolean): void {
  try {
    localStorage.setItem(REDACT_KEY, on ? "on" : "off");
  } catch {}
}

export function exportOptions(redact: boolean): ExportOptions {
  return { redact, notes: [] };
}

/** Copy an export (Markdown for a PR) or save it to Downloads, and say what happened. */
export async function deliver(file: Promise<ExportFile>, how: "copy" | "save", redact: boolean): Promise<boolean> {
  try {
    const f = await file;
    if (how === "copy") {
      await copyText(f.contents);
      toast(redact ? "Copied as Markdown, values redacted. Paste it into your PR" : "Copied as Markdown. Paste it into your PR");
    } else {
      const where = await (await api()).savePlanFile(f.name, f.contents);
      const shown = where.includes("/") || where.includes("\\") ? where.split(/[\\/]/).slice(-2).join("/") : where;
      toast(`Saved ${shown}`);
    }
    return true;
  } catch (e) {
    toast(errorMessage(e).message);
    return false;
  }
}
