import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import { DISCOVERY_LIMIT, discoveryKey, discoveryPage, preselectedDiscoveries } from "./discovery-results";

function check(ok: boolean, message: string) { if (!ok) throw new Error(message); }
const database = (i: number): DiscoveredDatabase => ({
  alias: `db-${i}`, cloud: "azure", engine: "postgres", environment: "development", server: "demo",
  host: "demo.example.com", database: `db-${i}`, location: "westeurope", subscription: "demo", resource_group: "demo",
  port: 5432, user: null, region: null, auth: "entra", saved_as: null,
});
const inventory = Array.from({ length: 10_000 }, (_, i) => database(i));
const page = discoveryPage(inventory);
check(page.total === 10_000 && page.items.length === DISCOVERY_LIMIT, "Large inventories must retain only the visible result budget");
check(page.items.every((item, i) => item === inventory[i]), "Discovery must preserve the cloud's result order");
check(Object.keys(preselectedDiscoveries(page, "demo")).length === 0, "A broad search must not preselect a truncated inventory");
// The only fresh database can be below the display boundary: it must never become a hidden selection.
const mostlySaved = inventory.map((item, i) => ({ ...item, saved_as: i === 9999 ? null : item.alias }));
check(Object.keys(preselectedDiscoveries(discoveryPage(mostlySaved), "demo")).length === 0, "Hidden fresh databases must not be preselected");
// A refined cloud search may return a database beyond the first page of the original inventory.
const refined = discoveryPage([inventory[9999]!, { ...database(10000), environment: "production" }, { ...database(10001), saved_as: "already-saved" }]);
const selected = preselectedDiscoveries(refined, "db-9999");
check(Object.keys(selected).length === 1 && selected[discoveryKey(inventory[9999]!)] === true, "Refined searches should select only fresh non-production matches");
check(Object.keys(preselectedDiscoveries(refined, " ")).length === 0, "Unfiltered discovery must never preselect connections");
check(Object.keys(preselectedDiscoveries(discoveryPage(inventory.slice(0, 11)), "demo")).length === 0, "More than ten fresh matches require explicit selection");
check(discoveryPage([]).total === 0 && discoveryPage([]).items.length === 0, "Empty results must stay empty");
console.log("Discovery results: 10,000 matches, bounded retention, refined searches and visible-only selection passed");
