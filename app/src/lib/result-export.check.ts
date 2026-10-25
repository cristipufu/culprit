import type { ResultSet } from "../gen/ResultSet";
import { csvCell, resultFile } from "./result-export";

function check(ok: boolean, message: string) { if (!ok) throw new Error(message); }
const result: ResultSet = {
  columns: ["id", "name", "name", "payload", "missing", "enabled"],
  rows: [[-12, 'é, "quoted"\nnext line', "", { a: [1, false, null] }, null, false], [0, "=1+2", "@SUM(1)", '{"big":9223372036854775807}', null, true]],
  truncated: true, elapsed_ms: 1, messages: [],
};
const csv = await resultFile(result, "csv", new Date("2026-10-25T12:34:56Z"));
check(csv.name === "query-results-2026-10-25T12-34-56Z.csv", "Export filenames must be portable and timestamped");
check(csv.contents === '\uFEFF"id","name","name","payload","missing","enabled"\r\n-12,"é, ""quoted""\nnext line","","{""a"":[1,false,null]}",,false\r\n0,"\t=1+2","\t@SUM(1)","{""big"":9223372036854775807}",,true\r\n', "CSV lost quotes, lines, Unicode, NULL, numeric or boolean values");
check(csv.protectedCells === 2, "CSV must report escaped text cells");
for (const text of ["=1", "+cmd", "-12", "@SUM(1)", "  =1", "\ttext", "\rtext", "\ntext", "＝1", "＋1", "－1", "＠SUM(1)"]) {
  const cell = csvCell(text);
  check(cell.protected && cell.text.startsWith('"\t'), "Formula-like text must be escaped inside the quoted CSV field");
}
for (const value of [null, -12, false, "email@example.com", "plain", ""]) check(!csvCell(value).protected, "Ordinary values must remain unchanged");
const header = await resultFile({ ...result, columns: ["=formula"], rows: [] }, "csv");
check(header.contents === '\uFEFF"\t=formula"\r\n' && header.protectedCells === 1, "Headers need the same CSV protection as values");
const json = await resultFile(result, "json");
const parsed = JSON.parse(json.contents);
check(JSON.stringify(parsed) === JSON.stringify({ columns: result.columns, rows: result.rows, truncated: true }), "JSON must retain duplicate column names, native values, precise JSON text and truncation");
check(json.protectedCells === 0 && parsed.rows[1][1] === "=1+2", "JSON must preserve formula-like strings verbatim");
const empty = JSON.parse((await resultFile({ ...result, rows: [], truncated: false }, "json")).contents);
check(empty.rows.length === 0 && !empty.truncated && empty.columns.length === 6, "Empty exports must keep their headers");
for (const format of ["csv", "json"] as const) {
  let failed = false;
  try { await resultFile({ ...result, rows: [[1]] }, format); } catch { failed = true; }
  check(failed, "Malformed row shapes must fail instead of silently shifting columns");
}
const large = { ...result, columns: ["id"], rows: Array.from({ length: 10_000 }, (_, i) => [i + 1]) };
let yielded = false;
setTimeout(() => { yielded = true; }, 0);
const all = await resultFile(large, "json");
check(yielded && JSON.parse(all.contents).rows.at(-1)[0] === 10_000, "Large exports must yield and include rows beyond the current grid page");
const deep = { ...result, columns: ["json"], rows: [[JSON.parse("[".repeat(100) + "0" + "]".repeat(100))]] };
check((await resultFile(deep, "json")).contents.length < 300, "Nested values must not amplify indentation");
console.log("Result exports: CSV escaping, formula-like text, duplicate columns, native JSON types, empty/malformed rows and 10,000-row batching passed");
