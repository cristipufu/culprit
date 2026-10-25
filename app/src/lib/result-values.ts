import type { ResultSet } from "../gen/ResultSet";

export type ResultCell = { row: number; col: number };
export const VALUE_PREVIEW_CHARS = 64_000;
const FORMAT_INPUT_CHARS = 256_000;
const FORMAT_OUTPUT_CHARS = 1_000_000;
const FORMAT_DEPTH = 32;

export function cellText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  return typeof value === "object" ? JSON.stringify(value) : String(value);
}

export function columnType(types: Record<string, string> | undefined, name: string): string | undefined {
  const key = name.toLowerCase();
  return types && Object.prototype.hasOwnProperty.call(types, key) ? types[key] : undefined;
}

/** Preserve the original number literals and escapes rather than reserializing decoded JSON values. */
export function formattedJson(raw: string): string | null {
  if (raw.length > FORMAT_INPUT_CHARS || !/^[\s]*[\[{]/.test(raw)) return null;
  const parts: string[] = [];
  let size = 0, depth = 0, quoted = false, escaped = false, last = "";
  const append = (text: string) => { size += text.length; parts.push(text); };
  const newline = () => append("\n" + "  ".repeat(depth));
  for (let i = 0; i < raw.length; i++) {
    const char = raw[i]!;
    if (quoted) {
      append(char);
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') { quoted = false; last = '"'; }
    } else if (char === '"') {
      quoted = true; append(char);
    } else if (char === "{" || char === "[") {
      if (++depth > FORMAT_DEPTH) return null;
      append(char);
      let next = i + 1;
      while (next < raw.length && /\s/.test(raw[next]!)) next++;
      if (raw[next] !== (char === "{" ? "}" : "]")) newline();
      last = char;
    } else if (char === "}" || char === "]") {
      if (--depth < 0) return null;
      if (last !== (char === "}" ? "{" : "[")) newline();
      append(char); last = char;
    } else if (char === ",") {
      append(char); newline(); last = char;
    } else if (char === ":") {
      append(": "); last = char;
    } else if (!/\s/.test(char)) {
      append(char); last = char;
    }
    if (size > FORMAT_OUTPUT_CHARS) return null;
  }
  try { JSON.parse(raw); } catch { return null; }
  return parts.join("");
}

export function valuePreview(text: string, limit = VALUE_PREVIEW_CHARS): string {
  const end = text.length > limit && /[\uD800-\uDBFF]/.test(text[limit - 1] ?? "") ? limit - 1 : limit;
  return text.slice(0, end);
}

export function rowTsv(result: ResultSet, row: number): string {
  return result.columns.map((_, col) => cellText(result.rows[row]?.[col])).join("\t");
}

/** Bound mounted cells as well as rows, including unusually wide results. */
export function resultPageSize(columns: number): number {
  return Math.max(1, Math.min(100, Math.floor(2000 / Math.max(1, columns))));
}
