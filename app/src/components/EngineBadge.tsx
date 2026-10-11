import type { Engine } from "../gen/Engine";
import type { Environment } from "../gen/Environment";

const ENGINE: Record<Engine, { abbr: string; label: string }> = {
  mssql: { abbr: "MS", label: "SQL Server" },
  postgres: { abbr: "Pg", label: "PostgreSQL" },
  mysql: { abbr: "My", label: "MySQL" },
};

/**
 * The database engine at a glance: a small badge in the engine's color with its initials, and the environment as a
 * dot on its corner (production stays unmistakable).
 */
export function EngineBadge({ engine, environment, size = "sm" }: { engine: Engine; environment?: Environment; size?: "sm" | "md" }) {
  const { abbr, label } = ENGINE[engine];
  const title = environment ? `${label} · ${environment}` : label;
  return (
    <span className={`engine-badge engine-${engine} size-${size}`} title={title} role="img" aria-label={title}>
      {abbr}
      {environment && <span className={`engine-env env-${environment}`} />}
    </span>
  );
}
