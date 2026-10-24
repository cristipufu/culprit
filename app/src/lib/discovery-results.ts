import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";

export const DISCOVERY_LIMIT = 20;
const PRESELECT_LIMIT = 10;

export type DiscoveryPage = { items: DiscoveredDatabase[]; total: number };

/** Retain a bounded prefix for display; the next cloud search still searches the full inventory. */
export function discoveryPage(items: DiscoveredDatabase[]): DiscoveryPage {
  return { items: items.slice(0, DISCOVERY_LIMIT), total: items.length };
}

export const discoveryKey = (database: DiscoveredDatabase) => `${database.cloud}/${database.host}/${database.database}`;

/** Only a small, fully visible search may preselect fresh, non-production connections. */
export function preselectedDiscoveries(page: DiscoveryPage, terms: string): Record<string, boolean> {
  if (!terms.trim() || page.total > DISCOVERY_LIMIT) return {};
  const candidates = page.items.filter((database) => !database.saved_as && database.environment !== "production");
  return candidates.length <= PRESELECT_LIMIT ? Object.fromEntries(candidates.map((database) => [discoveryKey(database), true])) : {};
}
