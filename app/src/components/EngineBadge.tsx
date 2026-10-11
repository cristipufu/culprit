import type { Engine } from "../gen/Engine";
import type { Environment } from "../gen/Environment";
import { isMariadb } from "../lib/engines";

const ENGINE: Record<Engine, { abbr: string; label: string }> = {
  mssql: { abbr: "MS", label: "SQL Server" },
  postgres: { abbr: "Pg", label: "PostgreSQL" },
  mysql: { abbr: "My", label: "MySQL" },
};

/**
 * The database engine at a glance: a small badge in the engine's color with its initials, and the environment as a
 * dot on its corner (production stays unmistakable). MariaDB rides on the MySQL engine; once a scan has named the
 * server (`product`), it gets its own badge.
 */
export function EngineBadge({
  engine,
  environment,
  product,
  size = "sm",
}: {
  engine: Engine;
  environment?: Environment;
  product?: string | null;
  size?: "sm" | "md";
}) {
  const mariadb = isMariadb(engine, product);
  const { abbr, label } = mariadb ? { abbr: "Ma", label: "MariaDB" } : ENGINE[engine];
  const title = environment ? `${label} · ${environment}` : label;
  return (
    <span className={`engine-badge engine-${mariadb ? "mariadb" : engine} size-${size}`} title={title} role="img" aria-label={title}>
      {abbr}
      {environment && <span className={`engine-env env-${environment}`} />}
    </span>
  );
}
