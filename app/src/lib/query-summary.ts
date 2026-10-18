/**
 * A statement in a few words: its verb, the main table and the key predicate ("SELECT customers · email = @P1"), so a
 * list of queries reads like a list of things the application does. Database text, rendered as text only.
 */
export type QuerySummary = { verb: string; target: string | null; predicate: string | null };

const VERBS = ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "WITH", "EXEC", "EXECUTE", "CALL", "CREATE", "ALTER", "DROP", "TRUNCATE", "BEGIN", "COMMIT", "SET", "SHOW", "VALUES", "TABLE"];

/** Comments out, whitespace folded, string literals kept short. */
function clean(text: string): string {
  return text
    .replace(/--[^\n]*/g, " ")
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .replace(/'(?:[^']|'')*'/g, (m) => (m.length > 18 ? `${m.slice(0, 14)}…'` : m))
    .replace(/\s+/g, " ")
    .trim();
}

/** `[shop].[customers]`, `"public"."orders"`, `` `shop`.`items` `` → `customers`. */
function tableName(raw: string): string {
  const parts = raw.split(/\s*\.\s*/).map((p) => p.replace(/^[[`"]|[\]`"]$/g, ""));
  return parts[parts.length - 1] ?? raw;
}

const PART = String.raw`(?:[\w$#@]+|\[[^\]]+\]|"[^"]+"|` + "`[^`]+`" + ")";
const IDENT = `(${PART}(?:\\s*\\.\\s*${PART})*)`;

function first(text: string, pattern: string): string | null {
  const m = new RegExp(pattern, "i").exec(text);
  return m?.[1] ? tableName(m[1]) : null;
}

/** The WHERE clause up to the next clause, shortened, identifiers unquoted. */
function predicate(text: string): string | null {
  const m = /\bWHERE\b(.*?)(?:\bGROUP\s+BY\b|\bORDER\s+BY\b|\bLIMIT\b|\bOFFSET\b|\bFETCH\b|\bOPTION\s*\(|\bFOR\s+UPDATE\b|\bRETURNING\b|\bUNION\b|;|$)/i.exec(text);
  const where = m?.[1]?.trim();
  if (!where) return null;
  const short = where
    .replace(/\[([^\]]+)\]/g, "$1")
    .replace(/[`"]([\w$]+)[`"]/g, "$1")
    .replace(/(\w)\s*\.\s*(\w)/g, "$1.$2")
    .replace(/\s*([=<>!]+)\s*/g, " $1 ")
    .replace(/\s+/g, " ");
  return short.length > 48 ? `${short.slice(0, 46).trimEnd()}…` : short;
}

export function querySummary(text: string, objectName?: string | null): QuerySummary {
  let body = clean(text);
  // A parameterized batch keeps its declarations in front: "(@P1 int)SELECT …".
  body = body.replace(/^\((@\w+[^)]*)\)\s*/, "");
  if (
    /^UPDATE\s+STATISTICS\b/i.test(body)
    && !new RegExp(String.raw`^UPDATE\s+STATISTICS\s+(?:SET\b|(?:AS\s+)?` + IDENT + String.raw`\s+SET\b)`, "i").test(body)
  ) {
    return { verb: "UPDATE STATISTICS", target: first(body, String.raw`^UPDATE\s+STATISTICS\s+` + IDENT), predicate: null };
  }
  // Skip CTE definitions to reach the statement they feed.
  if (/^WITH\b/i.test(body)) {
    let depth = 0;
    for (let i = 0; i < body.length; i++) {
      const c = body[i];
      if (c === "(") depth++;
      else if (c === ")") depth--;
      else if (depth === 0 && i > 4 && /^\s*(SELECT|INSERT|UPDATE|DELETE|MERGE)\b/i.test(body.slice(i))) {
        body = body.slice(i).trim();
        break;
      }
    }
  }
  const verbMatch = /^\(?\s*([A-Za-z]+)/.exec(body);
  const word = (verbMatch?.[1] ?? "").toUpperCase();
  const verb = VERBS.includes(word) ? word : word || "SQL";
  let target: string | null = null;
  switch (verb) {
    case "SELECT":
      target = first(body, String.raw`\bFROM\s+` + IDENT);
      break;
    case "UPDATE":
      target = first(body, String.raw`^UPDATE\s+(?:TOP\b\s*(?:\([^)]*\)|\d+)\s+(?:PERCENT\s+)?)?` + IDENT);
      break;
    case "INSERT":
      target = first(body, String.raw`\bINTO\s+` + IDENT);
      break;
    case "DELETE":
      target = first(body, String.raw`\bFROM\s+` + IDENT) ?? first(body, String.raw`^DELETE\s+` + IDENT);
      break;
    case "MERGE":
      target = first(body, String.raw`\bINTO\s+` + IDENT) ?? first(body, String.raw`^MERGE\s+` + IDENT);
      break;
    case "EXEC":
    case "EXECUTE":
    case "CALL":
      target = first(body, String.raw`^(?:EXEC|EXECUTE|CALL)\s+` + IDENT);
      break;
  }
  // A statement without a table still calls something people recognise: `SELECT pg_sleep($1)`, `DO SLEEP(?)`.
  if (!target && (verb === "SELECT" || verb === "DO")) {
    const call = /^(?:SELECT|DO)\s+(?:DISTINCT\s+)?([\w$]+(?:\.[\w$]+)?)\s*\(/i.exec(body);
    if (call?.[1]) target = `${call[1]}(…)`;
  }
  if (!target && objectName) target = tableName(objectName);
  return { verb, target, predicate: ["SELECT", "UPDATE", "DELETE"].includes(verb) ? predicate(body) : null };
}
