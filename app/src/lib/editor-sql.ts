import type { Engine } from "../gen/Engine";
import type { QueryStat } from "../gen/QueryStat";

const DOLLAR_TAG_RE = /^\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$/;
const GO_LINE_RE = /^[ \t]*GO(?:[ \t]+\d+)?[ \t]*(?:--[^\r\n]*)?(?:\r?\n|$)/i;
const TEMPLATE_PLACEHOLDER_RE = /<(?:app_role|table|column|the statement[^>]*|[a-z]+_[a-z_]+)>/i;

/** Strip non-SQL `[mysqld]` INI configuration sections included in some MySQL config recommendations. */
export function stripMyCnfBlocks(sql: string): string {
  const lines = sql.split("\n");
  const out: string[] = [];
  let inIniSection = false;
  for (const line of lines) {
    const trimmed = line.trim();
    if (/^\[mysqld\]$/i.test(trimmed)) {
      inIniSection = true;
      continue;
    }
    if (inIniSection) {
      if (trimmed === "" || trimmed.startsWith("--") || trimmed.startsWith("/*")) {
        inIniSection = false;
        out.push(line);
        continue;
      }
      if (/^[a-z0-9_]+\s*=\s*[^;]+$/i.test(trimmed)) {
        continue;
      }
      inIniSection = false;
    }
    out.push(line);
  }
  return out.join("\n");
}

function stripCommentsAndStrings(sql: string): string {
  return sql
    .replace(/--.*$/gm, " ")
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .replace(/'(?:''|\\'|[^'])*'/g, "''");
}

/** True when a statement contains unedited template placeholders (`<app_role>`, `<table>`, `…`) or is a bare `OPTION (...)` hint. */
export function isTemplateOrFragmentStatement(stmt: string): boolean {
  const stripped = stripCommentsAndStrings(stmt).trim();
  if (!stripped) return true;
  if (TEMPLATE_PLACEHOLDER_RE.test(stripped) || stripped.includes("…")) return true;
  if (/^OPTION\s*\(/i.test(stripped)) return true;
  return false;
}

/** True when a SQL slice contains at least one non-whitespace, non-comment character. */
export function hasExecutableSql(sql: string): boolean {
  let i = 0;
  while (i < sql.length) {
    const ch = sql[i]!;
    const next = sql[i + 1];
    if (/\s/.test(ch) || ch === ";") {
      i += 1;
      continue;
    }
    if (ch === "-" && next === "-") {
      const nl = sql.indexOf("\n", i + 2);
      if (nl === -1) return false;
      i = nl + 1;
      continue;
    }
    if (ch === "/" && next === "*") {
      const end = sql.indexOf("*/", i + 2);
      if (end === -1) return false;
      i = end + 2;
      continue;
    }
    return true;
  }
  return false;
}

/**
 * Split a SQL script or selection into individual top-level `;`- or `GO`-separated statements,
 * respecting single/double/backtick/bracket quotes, PostgreSQL dollar-quoting, and comments.
 * Segments that contain only comments, `[mysqld]` INI blocks, or unedited `<placeholder>` templates are omitted.
 */
export function splitSqlStatements(rawText: string): string[] {
  const docText = stripMyCnfBlocks(rawText);
  const statements: string[] = [];
  let inSingle = false;
  let inDouble = false;
  let inBacktick = false;
  let inBracket = false;
  let dollarTag: string | null = null;
  let start = 0;

  const pushSlice = (rawSlice: string) => {
    const slice = rawSlice.trim();
    if (hasExecutableSql(slice) && !isTemplateOrFragmentStatement(slice)) {
      statements.push(slice);
    }
  };

  for (let i = 0; i < docText.length; i++) {
    const ch = docText[i]!;
    const next = docText[i + 1];
    if (dollarTag !== null) {
      if (ch === "$" && docText.startsWith(dollarTag, i)) {
        i += dollarTag.length - 1;
        dollarTag = null;
      }
      continue;
    }
    if (inSingle && ch === "\\" && i + 1 < docText.length) {
      i += 1;
      continue;
    }
    if (!inSingle && !inDouble && !inBacktick && !inBracket) {
      if (i === 0 || docText[i - 1] === "\n") {
        const goMatch = GO_LINE_RE.exec(docText.slice(i));
        if (goMatch) {
          pushSlice(docText.slice(start, i));
          i += goMatch[0].length - 1;
          start = i + 1;
          continue;
        }
      }
      if (ch === "$") {
        const m = DOLLAR_TAG_RE.exec(docText.slice(i));
        if (m) {
          dollarTag = m[0];
          i += dollarTag.length - 1;
          continue;
        }
      }
      if (ch === "-" && next === "-") {
        const nl = docText.indexOf("\n", i + 2);
        if (nl === -1) break;
        i = nl;
        continue;
      }
      if (ch === "/" && next === "*") {
        const end = docText.indexOf("*/", i + 2);
        if (end === -1) break;
        i = end + 1;
        continue;
      }
      if (ch === ";") {
        pushSlice(docText.slice(start, i));
        start = i + 1;
        continue;
      }
    }
    if (ch === "'" && !inDouble && !inBacktick && !inBracket) inSingle = !inSingle;
    else if (ch === '"' && !inSingle && !inBacktick && !inBracket) inDouble = !inDouble;
    else if (ch === "`" && !inSingle && !inDouble && !inBracket) inBacktick = !inBacktick;
    else if (ch === "[" && !inSingle && !inDouble && !inBacktick && !inBracket) inBracket = true;
    else if (ch === "]" && inBracket) inBracket = false;
  }

  pushSlice(docText.slice(start));
  return statements;
}

const MAX_EDITABLE_PLACEHOLDERS = 10;

/** Split off a leading SQL Server parameter declaration prefix (`(@P1 int, @P2 nvarchar(4000)) SELECT ...`). */
export function splitLeadingParameterList(text: string): { parameters: string | null; body: string } {
  const trimmed = text.trimStart();
  if (!/^\(\s*@/.test(trimmed)) return { parameters: null, body: text };
  let depth = 0;
  let inSingle = false;
  for (let i = 0; i < trimmed.length; i++) {
    const ch = trimmed[i]!;
    if (ch === "'") inSingle = !inSingle;
    else if (!inSingle) {
      if (ch === "(") depth += 1;
      else if (ch === ")") {
        depth -= 1;
        if (depth === 0) {
          const params = trimmed.slice(1, i).trim();
          const rest = trimmed.slice(i + 1).trimStart();
          if (params && rest) return { parameters: params, body: rest };
          break;
        }
      }
    }
  }
  return { parameters: null, body: text };
}

/** Count unbound workload placeholders (`@par0`, `@P1`, `$1`, `?`) outside string literals and comments. */
export function countPlaceholders(text: string): number {
  const { body } = splitLeadingParameterList(text);
  const stripped = stripCommentsAndStrings(body);
  const named = new Set<string>();
  for (const m of stripped.matchAll(/(?:\$\d+|@(?:par|p|P)\d+)\b/g)) {
    named.add(m[0].toLowerCase());
  }
  const qMarks = [...stripped.matchAll(/(?:^|[\s(,=<>])\?(?=$|[\s),;])/g)].length;
  return named.size + qMarks;
}

function leadingVerb(text: string): string {
  const stripped = stripCommentsAndStrings(text).trim();
  const { body } = splitLeadingParameterList(stripped);
  const m = /^([A-Za-z]+)/.exec(body.trim());
  return m?.[1]?.toUpperCase() ?? "";
}

function isWriteStatement(text: string): boolean {
  const verb = leadingVerb(text);
  if (["DELETE", "UPDATE", "INSERT", "MERGE", "TRUNCATE", "CREATE", "ALTER", "DROP"].includes(verb)) {
    return true;
  }
  if (verb === "WITH") {
    return /\)\s*(DELETE|UPDATE|INSERT|MERGE)\b/i.test(text);
  }
  return false;
}

/**
 * Whether a recommendation / fix SQL block should offer "Open in editor":
 * - Must contain at least one runnable SQL statement (not a pure comment, `[mysqld]` INI snippet, `OPTION (...)`
 *   query-hint fragment, or unedited `<placeholder>` / `…` template).
 * - On SQLite (which Culprit opens strictly read-only), only read statements (`SELECT`, `WITH`, `VALUES`, `EXPLAIN`)
 *   can run in the editor.
 */
export function canOpenFixInEditor(sql: string | null | undefined, engine?: Engine): boolean {
  if (!sql) return false;
  const stmts = splitSqlStatements(sql);
  if (stmts.length === 0) return false;
  if (engine === "sqlite") {
    return stmts.every((s) => ["SELECT", "WITH", "VALUES", "EXPLAIN"].includes(leadingVerb(s)));
  }
  return true;
}

/**
 * Whether a captured workload query should offer "Open in editor":
 * - Parameterized write statements (`DELETE`/`UPDATE`/`INSERT`/`MERGE` with `@par0`, `$1`, `?`) can't and shouldn't be
 *   run ad-hoc from the editor.
 * - Read statements with more than 10 unbound placeholders remaining (after substituting any compiled parameter values
 *   from the captured plan) aren't practical to fill in by hand in the editor.
 */
export function canOpenQueryInEditor(rawText: string, preparedText?: string): boolean {
  const rawPlaceholders = countPlaceholders(rawText);
  if (rawPlaceholders === 0) return true;
  if (isWriteStatement(rawText)) return false;
  const remaining = preparedText !== undefined ? countPlaceholders(preparedText) : rawPlaceholders;
  return remaining <= MAX_EDITABLE_PLACEHOLDERS;
}

function splitTopLevelCommas(input: string): string[] {
  const parts: string[] = [];
  let depth = 0;
  let inSingle = false;
  let start = 0;
  for (let i = 0; i < input.length; i++) {
    const ch = input[i]!;
    if (ch === "'") inSingle = !inSingle;
    else if (!inSingle) {
      if (ch === "(") depth += 1;
      else if (ch === ")" && depth > 0) depth -= 1;
      else if (ch === "," && depth === 0) {
        parts.push(input.slice(start, i).trim());
        start = i + 1;
      }
    }
  }
  const tail = input.slice(start).trim();
  if (tail) parts.push(tail);
  return parts;
}

function parseCompiledValues(compiled: string | null | undefined): Map<string, string> {
  const map = new Map<string, string>();
  if (!compiled) return map;
  for (const part of splitTopLevelCommas(compiled)) {
    const eq = part.indexOf("=");
    if (eq === -1) continue;
    const name = part.slice(0, eq).trim().toLowerCase();
    let val = part.slice(eq + 1).trim();
    if (val.startsWith("(") && val.endsWith(")")) {
      val = val.slice(1, -1).trim();
    }
    if (name && val) map.set(name, val);
  }
  return map;
}

export function prepareRawQueryTextForEditor(text: string): string {
  const { parameters, body } = splitLeadingParameterList(text);
  if (parameters) {
    return `-- Parameters (${parameters}): replace @par… with values before running\n${body}`;
  }
  if (countPlaceholders(body) > 0) {
    return `-- Replace the parameter placeholders (?, $1, @p1) with literal values before running\n${body}`;
  }
  return body;
}

/**
 * Prepare a captured workload query for the editor:
 * - If the plan recorded compiled parameter values (`@par0 = N'...'`), substitute them into the statement so it can
 *   be explained or run right away.
 * - Otherwise, if parameter declarations or placeholders remain, add a short header comment noting the parameters
 *   to fill in.
 */
export function prepareQueryForEditor(query: QueryStat, engine: Engine): string {
  const { parameters: leadingParams, body } = splitLeadingParameterList(query.text);
  const params = query.parameters ?? leadingParams;
  const compiled =
    query.plan?.facts?.compiled_parameters ??
    query.plans.find((p) => p.compiled)?.compiled ??
    query.plan?.props?.Parameters;
  const values = engine === "mssql" ? parseCompiledValues(compiled) : new Map<string, string>();
  if (values.size > 0) {
    const replaced = body.replace(/(?<![@A-Za-z0-9_])@[A-Za-z0-9_]+\b/g, (token) => values.get(token.toLowerCase()) ?? token);
    if (replaced !== body) {
      return `-- Values filled from the plan's compiled parameters (${compiled})\n${replaced}`;
    }
  }
  if (params) {
    return `-- Parameters (${params}): replace @par… with values before running\n${body}`;
  }
  if (countPlaceholders(body) > 0) {
    return `-- Replace the parameter placeholders (?, $1, @p1) with literal values before running\n${body}`;
  }
  return body;
}
