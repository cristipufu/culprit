import { cellText, columnType, formattedJson, resultPageSize, VALUE_PREVIEW_CHARS, valuePreview } from "./result-values";

function check(ok: boolean, message: string) { if (!ok) throw new Error(message); }
for (const value of [{ a: [1, null, false, "a\n\"b\\c"], empty: {}, list: [] }, ["é", "👋", { "<script>": "</script>" }], {}]) {
  const raw = JSON.stringify(value);
  const pretty = formattedJson(raw);
  check(pretty !== null && JSON.stringify(JSON.parse(pretty)) === raw, "Formatting changed a JSON value");
}
const precise = '{"integer":9223372036854775807,"negativeZero":-0,"exponent":1e400,"escaped":"\\u0061","duplicate":1,"duplicate":2}';
const pretty = formattedJson(precise)!;
check(pretty.includes("9223372036854775807") && pretty.includes("-0") && pretty.includes("1e400") && pretty.includes("\\u0061"), "Formatting must retain exact numeric and escaped tokens");
check((pretty.match(/"duplicate"/g) ?? []).length === 2, "Formatting must preserve duplicate JSON keys");
for (const invalid of ['{"a":}', '{"a":1 2}', '[1,]', 'plain\ntext', 'null', '42', '"text"']) check(formattedJson(invalid) === null, "Non-object or malformed JSON was formatted");
check(formattedJson('['.repeat(1000) + '0' + ']'.repeat(1000)) === null, "Deep JSON must not amplify indentation or overflow the stack");
check(formattedJson('{"long":"' + 'x'.repeat(300_000) + '"}') === null, "Large JSON should keep its raw view");
check(cellText(null) === "NULL" && cellText("") === "" && cellText(false) === "false" && cellText(0) === "0", "NULL, empty text, false and zero must remain distinct");
check(valuePreview("x".repeat(VALUE_PREVIEW_CHARS + 10)).length === VALUE_PREVIEW_CHARS, "Value previews must remain bounded");
check(valuePreview("a👋b", 2) === "a", "Preview boundaries must not split surrogate pairs");
check(columnType({}, "__proto__") === undefined && columnType({}, "constructor") === undefined, "Column names must not read object prototypes as SQL types");
check(columnType(Object.fromEntries([["__proto__", "text"]]), "__proto__") === "text", "An actual column type must retain prototype-like names");
for (const columns of [0, 1, 20, 21, 500, 4000]) {
  const page = resultPageSize(columns);
  check(page >= 1 && page <= 100 && (columns > 2000 || page * columns <= 2000), "Wide and tall grids must bound mounted cells");
}
console.log("Result values: lossless JSON formatting, malformed/deep/large values, Unicode and render budgets passed");
