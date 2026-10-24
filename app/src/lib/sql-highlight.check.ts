import { tokenize } from "./sql-highlight";

// Highlighting is presentation only: copying rendered YSQL casts, parameters and quoted text must preserve SQL.
for (const sql of [
  "SELECT $1::timestamptz, value::text FROM events",
  'SELECT :named, @owner, "quoted:name", $$body$$',
  "SELECT 'unclosed text, $standalone, @, :",
  "SELECT '🦊 café', 值 FROM 数据 -- symbols: $ @ : ?",
]) {
  if (tokenize(sql).map((token) => token.text).join("") !== sql) {
    throw new Error(`SQL highlighting changed its source: ${sql}`);
  }
}
console.log("SQL highlighting preserves native casts and all source characters");
