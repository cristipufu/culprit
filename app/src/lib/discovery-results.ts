import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { DiscoveryResult } from "../gen/DiscoveryResult";
import policy from "../gen/discovery.json";

export const DISCOVERY_LIMIT = policy.result_limit;
const PRESELECT_LIMIT = 10;

export type DiscoveryPage = DiscoveryResult;

/** The browser demo models the same bounded response as the backend, using generated policy. */
export function discoveryPage(items: DiscoveredDatabase[]): DiscoveryPage {
  return { items: items.slice(0, DISCOVERY_LIMIT), has_more: items.length > DISCOVERY_LIMIT, warnings: [] };
}

export const discoveryKey = (database: DiscoveredDatabase) => `${database.cloud}/${database.host}/${database.database}`;

/** Only a small, fully visible search may preselect fresh, non-production connections. */
export function preselectedDiscoveries(page: DiscoveryPage, terms: string): Record<string, boolean> {
  if (!terms.trim() || page.has_more || page.warnings.length > 0) return {};
  const candidates = page.items.filter((database) => !database.saved_as && database.environment !== "production");
  return candidates.length <= PRESELECT_LIMIT ? Object.fromEntries(candidates.map((database) => [discoveryKey(database), true])) : {};
}
