import type { IndexStat } from "../gen/IndexStat";
import type { TableStat } from "../gen/TableStat";
import { ddl } from "./schema-model";

const check = (condition: boolean, message: string) => { if (!condition) throw new Error(message); };
const table = { schema: "main", name: "search", kind: "virtual", definition: "CREATE VIRTUAL TABLE search USING fts5(title, tokenize = 'unicode61')" } as TableStat;
check(ddl("sqlite", table, [], [], []) === `${table.definition};`, "Virtual DDL must preserve its module and options");
check(!ddl("sqlite", { ...table, definition: null }, [], [], []).includes("CREATE TABLE"), "Missing virtual DDL must not become an ordinary table");

const ordinary = { ...table, name: "documents", kind: "table", definition: "CREATE TABLE documents(id INTEGER PRIMARY KEY, title TEXT UNIQUE)" };
const indexes = [
  { name: "sqlite_autoindex_documents_1", is_primary_key: false, definition: null },
  { name: "by_title", is_primary_key: false, definition: "CREATE INDEX by_title ON documents(title)" },
] as IndexStat[];
const sql = ddl("sqlite", ordinary, [], indexes, []);
check(sql.includes(`${ordinary.definition};`), "Original rowid and uniqueness constraints must survive");
check(sql.includes(`${indexes[1]!.definition};`), "Original table DDL must retain separately defined indexes");
check(!sql.includes("sqlite_autoindex"), "Constraint indexes must not become invalid standalone CREATE INDEX statements");
console.log("Schema DDL regressions passed");
