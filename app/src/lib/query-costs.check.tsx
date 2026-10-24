import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import demo from "../../../fixtures/pg-demo.report.json";
import type { Report } from "../gen/Report";
import { QueryCostsPanel } from "../components/BeyondTime";
import { ENGINES, engineLabel, statementMetrics } from "./engines";

function check(ok: boolean, message: string) {
  if (!ok) throw new Error(message);
}

const profiles = ENGINES.map((engine) => ({ engine, product: engineLabel(engine) }));
profiles.push({ engine: "mysql", product: "MariaDB 11.8.9" });
for (const { engine, product } of profiles) {
  const report = structuredClone(demo) as unknown as Report;
  report.snapshot.server.engine = engine;
  report.snapshot.server.product = product;
  const query = report.snapshot.workload!.queries[0]!;
  const metrics = statementMetrics(engine);
  query.costs = {
    plan_ms: 50, plans: 10, failed: 1, aborted: null, aborted_mean_ms: null, warnings: null,
    top_error: null, jit_ms: null, jit_functions: null, log_bytes: null, log_share: null,
    log_full_pages: null, row_bytes: null, compile_history: null,
  };
  const measured = renderToStaticMarkup(createElement(QueryCostsPanel, { report, query }));
  check(measured.includes(`>${metrics.planning_label}</span>`), `${engine}: wrong planning terminology`);
  check(measured.includes("Query Store counts errors") === !!metrics.errors_note, `${engine}: another source's error note leaked`);
  if (engine === "yugabyte") {
    check(measured.includes(">Planning</span>") && !measured.includes("Compiling"), "YSQL plans are not SQL Server compilations");
    check(measured.includes("keeps no error count or lock time per statement"), "YSQL must explain its missing statement counters");
  }
  query.costs.plan_ms = null;
  report.snapshot.settings = metrics.planning_setting
    ? [{ name: metrics.planning_setting, value: "off", default_value: "off", scope: "session" }]
    : [];
  const disabled = renderToStaticMarkup(createElement(QueryCostsPanel, { report, query }));
  check(disabled.includes("track_planning is off") === !!metrics.planning_setting, `${engine}: wrong planning setting explanation`);
  report.snapshot.settings = [];
  const unknown = renderToStaticMarkup(createElement(QueryCostsPanel, { report, query }));
  check(!unknown.includes("track_planning is off"), `${engine}: missing evidence was reported as an off setting`);
  query.text = "UPDATE public.orders SET status = 'paid'";
  query.costs.log_bytes = 102400;
  query.waits = [{ category: "Lock", wait_type: null, wait_ms: 1000, tasks: null }];
  const io = renderToStaticMarkup(createElement(QueryCostsPanel, { report, query }));
  check(io.includes(`>${metrics.log_label}</span>`), `${product}: wrong log terminology`);
  check(io.includes(">Lock waits</span>") === metrics.lock_time, `${product}: another source's lock timing leaked`);
  if (product.startsWith("MariaDB")) {
    check(io.includes("MariaDB keeps no planning time or redo bytes"), "MariaDB was described as MySQL");
  }
}
console.log(`Query cost rendering follows shared statement metrics for all ${profiles.length} engine/product profiles`);
