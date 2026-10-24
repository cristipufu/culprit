import type { DatabaseProduct } from "../gen/DatabaseProduct";
import type { Engine } from "../gen/Engine";
import type { EngineFacts } from "../gen/EngineFacts";
import engines from "../gen/engines.json";
import { databaseProduct, engineLabel, isMariadb, planEngineName, serverName } from "./engines";

function check(ok: boolean, message: string) {
  if (!ok) throw new Error(message);
}

const seen = new Set<DatabaseProduct>();
for (const [key, facts] of Object.entries(engines as Record<Engine, EngineFacts>)) {
  const engine = key as Engine;
  check(databaseProduct(engine) === facts.products[0]!.id, `${engine}: missing default product`);
  check(serverName(engine) === engineLabel(engine), `${engine}: inconsistent default label`);
  for (const product of facts.products) {
    check(!seen.has(product.id), `${product.id}: duplicate product identity`);
    seen.add(product.id);
    const recorded = `${product.prefix ?? product.label} 2026.1`;
    check(databaseProduct(engine, recorded) === product.id, `${recorded}: wrong identity`);
    check(serverName(engine, recorded) === product.label, `${recorded}: wrong label`);
    check(planEngineName({ engine, props: { flavor: product.prefix ?? product.label } }) === product.label, `${recorded}: wrong plan label`);
  }
}
check(seen.size === 7, "All seven database products must be exported by core");
check(databaseProduct("postgres", "MariaDB 11.8") === "postgres", "Product recognition must stay within its provider");
check(databaseProduct("mysql", "TimescaleDB 2.31") === "mysql", "An extension cannot select another provider");
check(isMariadb("mysql", "MariaDB 11.8"), "MariaDB must retain product-specific advice");
check(!isMariadb("postgres", "MariaDB 11.8"), "MariaDB advice must not cross providers");
console.log(`Product identity: ${seen.size} products, defaults, plan labels and provider boundaries passed`);
