import { querySummary } from "./query-summary";
const cases: [string, string, string][] = [
 ["UPDATE STATISTICS dbo.orders WITH FULLSCAN", "UPDATE STATISTICS", "orders"],
 ["UPDATE STATISTICS [dbo].[orders] (idx_orders) WITH SAMPLE 100 PERCENT", "UPDATE STATISTICS", "orders"],
 ["/* scheduled */ UPDATE STATISTICS [dbo].[order lines] WITH FULLSCAN", "UPDATE STATISTICS", "order lines"],
 ["-- UPDATE STATISTICS dbo.orders\nSELECT * FROM dbo.orders", "SELECT", "orders"],
 ["UPDATE TOP (1000) dbo.orders SET state = 1", "UPDATE", "orders"],
 ["UPDATE TOP (@limit) dbo.orders SET state = 1", "UPDATE", "orders"],
 ["UPDATE TOP 10 PERCENT dbo.orders SET state = 1", "UPDATE", "orders"],
 ["UPDATE top100 SET state = 1", "UPDATE", "top100"],
 ["UPDATE statistics SET value = 1", "UPDATE", "statistics"],
 ["UPDATE statistics s SET value = 1", "UPDATE", "statistics"],
 ["UPDATE statistics AS s SET value = 1", "UPDATE", "statistics"],
];
for (const [sql, verb, target] of cases) {
 const result = querySummary(sql);
 if (result.verb !== verb || result.target !== target) throw new Error(JSON.stringify({ sql, result, verb, target }));
}
console.log(`All ${cases.length} SQL naming review cases passed.`);
