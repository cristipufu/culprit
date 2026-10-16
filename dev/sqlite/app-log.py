"""Write the Rails development log an app on dev/sqlite/demo.db would: 90 minutes of requests, the same every time.

    python3 dev/sqlite/app-log.py > dev/sqlite/development.log

Culprit reads it as the demo file's statement history (`culprit conn set <name> --query-log dev/sqlite/development.log`).
Two listings filter orders by customer, which has no index (a scan of every order each time); a report of each region's
customers and their order counts makes SQLite build a throwaway index on orders.customer_id on every run; a customer
lookup by primary key is cheap and frequent; transactions wrap the occasional write.
"""

import random
from datetime import datetime, timedelta, timezone

random.seed(7)
start = datetime(2026, 10, 15, 9, 0, 0, tzinfo=timezone.utc)
lines = []
for i in range(1800):  # a request every 3 seconds
    at = start + timedelta(seconds=3 * i)
    customer = random.randint(1, 5000)
    stamp = at.strftime("%Y-%m-%d %H:%M:%S +0000")
    if i % 50 == 0:
        lines.append(f'Started POST "/orders" for 10.0.0.{i % 250} at {stamp}')
        lines.append("  TRANSACTION (0.1ms)  BEGIN")
        lines.append(
            f'  Order Create ({random.uniform(0.3, 0.9):.1f}ms)  INSERT INTO "orders" ("customer_id", "status", "total", "created_at") '
            f'VALUES (?, ?, ?, ?) RETURNING "id"  [["customer_id", {customer}], ["status", "pending"], ["total", 42.0], ["created_at", "{stamp}"]]'
        )
        lines.append(f"  TRANSACTION ({random.uniform(2, 6):.1f}ms)  COMMIT")
        lines.append("Completed 201 Created in 12ms (ActiveRecord: 6.0ms | Allocations: 800)")
        continue
    if i % 20 == 5:
        region = random.choice(["us", "eu", "apac"])
        lines.append(f'Started GET "/reports/customers?region={region}" for 10.0.0.{i % 250} at {stamp}')
        lines.append(
            f'  Customer Load ({random.uniform(60, 120):.1f}ms)  SELECT "customers"."email", COUNT("orders"."id") AS "orders_count" '
            f'FROM "customers" LEFT OUTER JOIN "orders" ON "orders"."customer_id" = "customers"."id" '
            f'WHERE "customers"."region" = ? GROUP BY "customers"."id"  [["region", "{region}"]]'
        )
        lines.append("Completed 200 OK in 120ms (Views: 9.0ms | ActiveRecord: 95.0ms | Allocations: 5100)")
        continue
    lines.append(f'Started GET "/customers/{customer}/orders" for 10.0.0.{i % 250} at {stamp}')
    lines.append(
        f'  Customer Load ({random.uniform(0.1, 0.3):.1f}ms)  SELECT "customers".* FROM "customers" WHERE "customers"."id" = ? LIMIT ?  [["id", {customer}], ["LIMIT", 1]]'
    )
    if i % 3 == 0:
        lines.append(
            f'  Order Load ({random.uniform(18, 30):.1f}ms)  SELECT "orders".* FROM "orders" WHERE "orders"."customer_id" = ? '
            f'AND "orders"."status" = ? ORDER BY "orders"."total" DESC LIMIT ?  [["customer_id", {customer}], ["status", "shipped"], ["LIMIT", 20]]'
        )
    else:
        lines.append(
            f'  Order Load ({random.uniform(15, 25):.1f}ms)  SELECT "orders".* FROM "orders" WHERE "orders"."customer_id" = ? '
            f'ORDER BY "orders"."created_at" DESC LIMIT ?  [["customer_id", {customer}], ["LIMIT", 20]]'
        )
    lines.append("Completed 200 OK in 30ms (Views: 4.1ms | ActiveRecord: 22.0ms | Allocations: 1200)")
print("\n".join(lines))
