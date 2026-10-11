import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Environment } from "../gen/Environment";

export const ENVIRONMENT_ORDER: Environment[] = ["production", "staging", "development", "local"];

export const ENVIRONMENT_LABEL: Record<Environment, string> = {
  production: "Production",
  staging: "Staging & alpha",
  development: "Development",
  local: "Local",
};

export function groupByEnvironment(connections: ConnectionInfo[]): { environment: Environment; items: ConnectionInfo[] }[] {
  return ENVIRONMENT_ORDER.map((environment) => ({
    environment,
    items: connections.filter((c) => c.environment === environment).sort((a, b) => a.name.localeCompare(b.name)),
  })).filter((g) => g.items.length > 0);
}
