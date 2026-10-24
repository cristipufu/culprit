import type { Engine } from "../gen/Engine";
import type { DatabaseProduct } from "../gen/DatabaseProduct";
import type { Environment } from "../gen/Environment";
import { databaseProduct, serverName } from "../lib/engines";
import mssqlLogo from "../../../assets/providers/mssql.svg";
import postgresLogo from "../../../assets/providers/postgres.svg";
import yugabyteLogo from "../../../assets/providers/yugabyte.svg";
import mysqlLogo from "../../../assets/providers/mysql.svg";
import mariadbLogo from "../../../assets/providers/mariadb.svg";
import sqliteLogo from "../../../assets/providers/sqlite.svg";
import timescaleLogo from "../../../assets/providers/timescale.svg";

/** The same local SVG assets are used by the marketing website. Labels still come from the engine facts. */
const LOGOS: Record<DatabaseProduct, string> = { mssql: mssqlLogo, postgres: postgresLogo, yugabyte: yugabyteLogo, mysql: mysqlLogo, mariadb: mariadbLogo, sqlite: sqliteLogo, timescale: timescaleLogo };

/**
 * The database engine's logo, with the environment as a dot on its corner (production stays unmistakable).
 * MariaDB rides on the MySQL engine; once a scan has named the
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
  const brand = databaseProduct(engine, product);
  const label = serverName(engine, product);
  const logo = LOGOS[brand];
  const title = environment ? `${label} · ${environment}` : label;
  return (
    <span className={`engine-badge engine-${brand} size-${size}`} title={title} role="img" aria-label={title}>
      <img src={logo} width={size === "md" ? 28 : 22} height={size === "md" ? 28 : 22} alt="" aria-hidden="true" draggable={false} />
      {environment && <span className={`engine-env env-${environment}`} />}
    </span>
  );
}
