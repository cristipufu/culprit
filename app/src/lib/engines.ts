import type { Engine } from "../gen/Engine";
import type { Plan } from "../gen/Plan";

const LABEL: Record<Engine, string> = { mssql: "SQL Server", postgres: "Postgres", mysql: "MySQL" };

/** MariaDB is served by the MySQL engine; the server's product string (or a plan's flavor) tells them apart. */
export function isMariadb(engine: Engine, product?: string | null): boolean {
  return engine === "mysql" && !!product?.startsWith("MariaDB");
}

/** The server's name in sentences: "SQL Server", "Postgres", "MySQL" or "MariaDB". */
export function serverName(engine: Engine, product?: string | null): string {
  return isMariadb(engine, product) ? "MariaDB" : LABEL[engine];
}

/** The engine a plan came from, telling MariaDB's plans from MySQL's. */
export function planEngineName(plan: Pick<Plan, "engine" | "props">): string {
  return plan.engine === "mysql" && plan.props?.flavor === "MariaDB" ? "MariaDB" : LABEL[plan.engine];
}
