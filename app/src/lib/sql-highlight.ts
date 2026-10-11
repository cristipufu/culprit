const KEYWORDS = new Set(
  "select from where and or not in is null as join inner left right full outer cross apply on group by order having union all distinct top limit offset fetch next rows only insert into values update set delete merge with case when then else end exists between like asc desc over partition count sum avg min max cast convert coalesce isnull returning create index nonclustered clustered unique include online concurrently table alter drop primary key foreign references begin commit rollback transaction declare exec execute option recompile nolock if".split(
    " ",
  ),
);

export type Token = { kind: "k" | "s" | "n" | "c" | "p" | "t"; text: string };

const PATTERN =
  /(--[^\n]*|\/\*[\s\S]*?\*\/)|('(?:[^']|'')*')|(\$\d+|@@?[A-Za-z_][\w]*|:[A-Za-z_]\w*|\?)|(\b\d+(?:\.\d+)?\b)|([A-Za-z_][\w$]*)|(\s+|[^\sA-Za-z_\d'$@:?-]+|-)/g;

export function tokenize(sql: string): Token[] {
  const out: Token[] = [];
  for (const m of sql.matchAll(PATTERN)) {
    const [text, comment, str, param, num, word] = m;
    if (comment) out.push({ kind: "c", text });
    else if (str) out.push({ kind: "s", text });
    else if (param) out.push({ kind: "p", text });
    else if (num) out.push({ kind: "n", text });
    else if (word && KEYWORDS.has(word.toLowerCase())) out.push({ kind: "k", text });
    else out.push({ kind: "t", text });
  }
  return out;
}

const BREAK_BEFORE = /\s+(FROM|WHERE|GROUP BY|ORDER BY|HAVING|UNION ALL|UNION|LEFT JOIN|RIGHT JOIN|INNER JOIN|FULL JOIN|CROSS APPLY|OUTER APPLY|JOIN|LIMIT|OFFSET|VALUES|SET|RETURNING|OPTION)\b/gi;

/** Light formatting for display: one clause per line. Leaves already-formatted SQL alone. */
export function prettySql(sql: string): string {
  const trimmed = sql.trim();
  if (trimmed.includes("\n")) return trimmed;
  return trimmed.replace(BREAK_BEFORE, (_, kw: string) => `\n${kw.toUpperCase()}`).replace(/\s+(AND|OR)\s+/gi, (_, op: string) => `\n  ${op.toUpperCase()} `);
}
