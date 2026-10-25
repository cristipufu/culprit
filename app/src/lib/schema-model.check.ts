import type { IndexStat } from "../gen/IndexStat";
import type { TableStat } from "../gen/TableStat";
import type { Report } from "../gen/Report";
import { coveredBy, ddl, nextColumnSort, previewSql, relations } from "./schema-model";
import { ENGINES } from "./engines";
import nativeSnapshot from "../../../fixtures/yugabyte-demo.json";

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
for (const kind of ["hypertable", "timescale-unresolved"]) {
  const managed = ddl("postgres", { ...ordinary, kind }, [], indexes, []);
  check(!managed.includes("CREATE TABLE") && managed.includes("TimescaleDB"), "Managed hypertables require a native schema export");
}
console.log("Schema DDL regressions passed");

const nativeTable = { ...ordinary, schema: "public", kind: "docdb", definition: null, storage: { docdb: { tablets: 3, hash_key_columns: 0, colocated: false, tablegroup: null } } } as TableStat;
const nativePk = { ...indexes[1], name: "documents_pkey", kind: "lsm", columns: ["id"], include: [], is_primary_key: true, docdb: { hash_columns: [], tablets: 3 } } as unknown as IndexStat;
const nativeSql = ddl("yugabyte", nativeTable, [], [nativePk], []);
check(nativeSql.includes('PRIMARY KEY ("id" ASC)'), "A range primary key must not silently become the YSQL default HASH key");
const hashPk = { ...nativePk, columns: ["tenant", "id", "created_at DESC"], docdb: { hash_columns: ["tenant", "id"], tablets: 3 } } as IndexStat;
check(ddl("yugabyte", nativeTable, [], [hashPk], []).includes('PRIMARY KEY (("tenant", "id") HASH, "created_at" DESC)'), "Composite hashes must stay one group");
check(!ddl("yugabyte", { ...nativeTable, storage: null }, [], [nativePk], []).includes("CREATE TABLE"), "Unknown native distribution cannot become a guessed recreation script");
const hashIndex = { ...nativePk, is_primary_key: false, name: "hash", docdb: { hash_columns: ["id"], tablets: 3 } } as IndexStat;
const rangeIndex = { ...hashIndex, name: "range", columns: ["id", "created_at"], docdb: { hash_columns: [], tablets: 3 } } as IndexStat;
check(coveredBy(hashIndex, [rangeIndex]) === null, "Ordered keys cannot establish redundancy of a hash index");
const nativeReport = { snapshot: structuredClone(nativeSnapshot) } as unknown as Report;
const orders = nativeReport.snapshot.tables.find((table) => table.name === "orders")!;
check(relations(nativeReport, orders).outgoing[0]?.indexed === false, "A complete catalog can establish a missing FK index");
delete nativeReport.snapshot.indexes[0]!.docdb;
// Unknown metadata on a different table does not invalidate this table's known index coverage.
check(relations(nativeReport, orders).outgoing[0]?.indexed === false, "FK coverage is scoped to its own table");
delete nativeReport.snapshot.indexes.find((index) => index.table === "orders")!.docdb;
check(relations(nativeReport, orders).outgoing[0]?.indexed === null, "Unknown hash keys cannot establish that an FK index is missing");

for (const engine of ENGINES) {
  const query = previewSql(engine, { schema: 'odd"`]schema', name: 'order' }, 1000, { filter: "id > 1 -- comment", order: { column: 'odd"`]column', direction: "desc" } });
  check(query.includes("WHERE (\nid > 1 -- comment\n)"), "A line comment must end before the predicate boundary");
  check(query.endsWith("LIMIT 1000") || query.includes("TOP (1000)"), "Each provider must request bounded server rows");
  check(query.includes(" DESC"), "A clicked header must request server ordering");
}
for (const engine of ENGINES) {
  const query = previewSql(engine, { schema: " schema ", name: " table " }, 1000, { filter: null, order: { column: " key ", direction: "asc" } });
  for (const identifier of [" schema ", " table ", " key "]) check(query.includes(identifier), "Preview SQL must preserve catalog identifiers verbatim");
}
const ascending = nextColumnSort(null, "id");
const descending = nextColumnSort(ascending, "id");
check(ascending?.direction === "asc" && descending?.direction === "desc" && nextColumnSort(descending, "id") === null, "Sorting cycles through ascending, descending, then unsorted");
check(nextColumnSort(descending, "other")?.direction === "asc", "A new column starts ascending");
console.log("Table previews: every provider, quoted names, bounded rows and sort transitions passed");
