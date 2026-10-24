import type { Ability as EngineAbility } from "../gen/Ability";
import type { Engine } from "../gen/Engine";
import type { DatabaseProduct } from "../gen/DatabaseProduct";
import type { EngineFacts } from "../gen/EngineFacts";
import type { Plan } from "../gen/Plan";
import type { SessionAction } from "../gen/SessionAction";
import engines from "../gen/engines.json";

/** Everything the app knows about each engine without a connection: its label, what it can answer, its session
 *  actions and why it refuses what it can't do. Written from the engine files in culprit-core (`engines/*.rs`) to
 *  `gen/engines.json` by its tests, so views, the desktop and the browser demo all answer as the engine does. */
const FACTS = engines as Record<Engine, EngineFacts>;

/** Every engine, in the order the engine files list them (`Engine::ALL`). */
export const ENGINES = Object.keys(FACTS) as Engine[];
export const sqlSyntax = (engine: Engine) => FACTS[engine].sql_syntax;
export const statementMetrics = (engine: Engine) => FACTS[engine].statement_metrics;
export const waitSampling = (engine: Engine) => FACTS[engine].wait_sampling;

function productIdentity(engine: Engine, product?: string | null) {
  const products = FACTS[engine].products;
  return products.find((p) => p.prefix !== null && product?.startsWith(p.prefix)) ?? products[0];
}

/** Branding follows the product the provider actually identified, without changing its execution engine. */
export function databaseProduct(engine: Engine, product?: string | null): DatabaseProduct {
  return productIdentity(engine, product)?.id ?? engine;
}

const LABEL = Object.fromEntries(Object.entries(FACTS).map(([e, f]) => [e, f.label])) as Record<Engine, string>;

/** The engine's name in sentences: "SQL Server", "PostgreSQL", "MySQL", "SQLite". */
export function engineLabel(engine: Engine): string {
  return LABEL[engine];
}

/** How a person connects to the engine: its name where they pick it, whether it's a file they choose, and what they
 *  paste otherwise. */
export function connectFacts(engine: Engine): EngineFacts["connect"] {
  return FACTS[engine].connect;
}

export function engineLimitations(engine: Engine): string[] { return FACTS[engine].limitations; }

/** MariaDB is served by the MySQL engine; the server's product string (or a plan's flavor) tells them apart. */
export function isMariadb(engine: Engine, product?: string | null): boolean {
  return databaseProduct(engine, product) === "mariadb";
}

export type Ability = EngineAbility;

export function can(engine: Engine, ability: Ability): boolean {
  return FACTS[engine].abilities[ability];
}

/** The error the engine gives for something it can't do, as the desktop returns it. */
export function refusal(engine: Engine, ability: Ability): { code: "unsupported"; message: string; hint: string | null } {
  const r = FACTS[engine].refusals[ability];
  return { code: "unsupported", message: r?.message ?? `${LABEL[engine]} can't do this`, hint: r?.hint ?? null };
}

/** What Live may do to another session on this engine, outside production. */
export function sessionActions(engine: Engine): SessionAction[] {
  return FACTS[engine].session_actions;
}

/** The engine's word for a statement that is waiting ("suspended" on SQL Server). */
export function waitingState(engine: Engine): string {
  return FACTS[engine].waiting_state;
}

export function measuresRuns(engine: Engine): boolean {
  return can(engine, "measures_runs");
}

/** The server's name in sentences: "SQL Server", "PostgreSQL", "MySQL" or "MariaDB". */
export function serverName(engine: Engine, product?: string | null): string {
  return productIdentity(engine, product)?.label ?? LABEL[engine];
}

/** The engine a plan came from, telling MariaDB's plans from MySQL's. */
export function planEngineName(plan: Pick<Plan, "engine" | "props">): string {
  return serverName(plan.engine, plan.props?.flavor);
}
