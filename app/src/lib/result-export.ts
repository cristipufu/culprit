import type { ResultSet } from "../gen/ResultSet";
import { cellText } from "./result-values";

export type ResultFileFormat = "csv" | "json";

/** Quote strings, including headers; prefix formula-like text inside the quoted CSV field for spreadsheets. */
export function csvCell(value: unknown): { text: string; protected: boolean } {
  if (value == null) return { text: "", protected: false };
  if (typeof value === "number" || typeof value === "boolean") return { text: String(value), protected: false };
  const raw = cellText(value);
  const protectedText = /^[\s\uFEFF]*[=+\-@＝＋－＠]|^[\t\r\n]/u.test(raw);
  return { text: '"' + (protectedText ? "\t" : "") + raw.replace(/"/g, '""') + '"', protected: protectedText };
}

/** Export the loaded result, independent of the displayed page. Yield between batches to keep controls responsive. */
export async function resultFile(result: ResultSet, format: ResultFileFormat, now = new Date()) {
  const name = `query-results-${now.toISOString().slice(0, 19).replace(/:/g, "-")}Z.${format}`;
  const chunks: string[] = [];
  let protectedCells = 0;
  const csvRow = (row: unknown[]) => row.map((value) => {
    const cell = csvCell(value);
    if (cell.protected) protectedCells++;
    return cell.text;
  }).join(",");
  if (format === "csv") chunks.push("\uFEFF" + csvRow(result.columns) + "\r\n");
  else chunks.push('{\n  "columns": ' + JSON.stringify(result.columns) + ',\n  "rows": [');
  for (let row = 0; row < result.rows.length; row++) {
    if (result.rows[row]!.length !== result.columns.length) throw new Error("Cannot export: a result row does not match its column headers");
    if (format === "csv") chunks.push(csvRow(result.rows[row]!) + "\r\n");
    // Compact nested values keep deeply nested JSON from amplifying indentation and memory use.
    else chunks.push((row ? "," : "") + "\n    " + JSON.stringify(result.rows[row]));
    if ((row + 1) % 128 === 0) await new Promise<void>((resolve) => setTimeout(resolve, 0));
  }
  if (format === "json") chunks.push((result.rows.length ? "\n  " : "") + '],\n  "truncated": ' + String(result.truncated) + "\n}\n");
  return { name, contents: chunks.join(""), protectedCells };
}
