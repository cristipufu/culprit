import { useMemo } from "react";
import { prettySql, tokenize } from "../lib/sql-highlight";

export function Sql({ text, pretty = true, className }: { text: string; pretty?: boolean; className?: string }) {
  const tokens = useMemo(() => tokenize(pretty ? prettySql(text) : text), [text, pretty]);
  return (
    <pre className={`sql ${className ?? ""}`}>
      {tokens.map((t, i) => (t.kind === "t" ? t.text : <span key={i} className={t.kind}>{t.text}</span>))}
    </pre>
  );
}
