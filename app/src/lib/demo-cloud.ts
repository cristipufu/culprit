import type { CloudPoint } from "../gen/CloudPoint";
import type { CloudSeries } from "../gen/CloudSeries";
import type { CloudSignals } from "../gen/CloudSignals";
import type { ThrottleEpisode } from "../gen/ThrottleEpisode";

/**
 * Synthetic cloud metrics for the browser demo: a burstable Amazon RDS instance that runs out of CPU credits, and an
 * Azure SQL database that hits its log-write limit. Shapes follow what CloudWatch and Azure Monitor return; nothing
 * here describes a real resource.
 */

const STEP = 5 * 60_000;

/** A smooth, repeatable wobble so the demo looks the same on every load. */
function wave(i: number, seed: number): number {
  return Math.sin(i / 7 + seed) * 0.5 + Math.sin(i / 2.3 + seed * 2) * 0.25;
}

function points(from: number, to: number, value: (t: number, i: number) => number): CloudPoint[] {
  const out: CloudPoint[] = [];
  const start = Math.ceil(from / STEP) * STEP;
  for (let t = start, i = 0; t < to; t += STEP, i++) out.push({ at: new Date(t).toISOString().replace(".000Z", "Z"), value: value(t, i) });
  return out;
}

function series(metric: string, label: string, unit: CloudSeries["unit"], pts: CloudPoint[], extra: Partial<CloudSeries> = {}): CloudSeries {
  return { metric, label, unit, points: pts, limit: null, limit_label: null, floor: false, ...extra };
}

function iso(t: number): string {
  return new Date(t).toISOString().replace(".000Z", "Z");
}

export function demoCloud(connection: string, from: number, to: number): CloudSignals | null {
  const span = to - from;
  // The episode sits two thirds into whatever period is shown.
  const a = from + span * 0.62;
  const b = a + Math.max(span * 0.1, 25 * 60_000);
  const inside = (t: number) => t >= a && t < b;
  const base = { start: iso(from), end: iso(to), period_seconds: 300, fetched_at: iso(Date.now()), notes: ["From CloudWatch, one point per 5 min."] };
  if (connection.startsWith("pg")) {
    let credits = 140;
    const creditPoints = points(from, to, (t) => {
      credits = inside(t) ? 0 : t < a ? Math.max(0, credits - 2.4) : Math.min(288, credits + 1.2);
      return credits;
    });
    const cpu = points(from, to, (t, i) => (inside(t) ? 20 + wave(i, 1) * 0.6 : 34 + wave(i, 1) * 14));
    const episodes: ThrottleEpisode[] = [
      {
        kind: "cpu-credits",
        start: iso(Math.ceil(a / STEP) * STEP),
        end: iso(Math.ceil(b / STEP) * STEP),
        metric: "CPUCreditBalance",
        worst: 0,
        title: "CPU credits ran out",
        detail: "The credit balance hit zero: the instance was held to its 20% CPU baseline, so every query waited longer for CPU.",
        ongoing: false,
      },
    ];
    return {
      ...base,
      provider: "aws",
      service: "Amazon RDS for PostgreSQL",
      resource: "demo-pg",
      tier: "db.t3.medium (burstable, 2 vCPUs) · gp2, 100 GB",
      series: [
        series("CPUUtilization", "CPU", "percent", cpu, { limit: 20, limit_label: "baseline 20%" }),
        series("CPUCreditBalance", "CPU credits", "credits", creditPoints, { floor: true }),
        series("IOPS", "IOPS", "iops", points(from, to, (_, i) => 180 + wave(i, 2) * 90), { limit: 300, limit_label: "baseline 300 IOPS (3 per GB)" }),
        series("BurstBalance", "Storage burst balance", "percent", points(from, to, (_, i) => 96 + wave(i, 3) * 3), { floor: true }),
        series("DiskLatency", "Disk latency", "ms", points(from, to, (t, i) => (inside(t) ? 6 : 1.6) + wave(i, 4))),
        series("DatabaseConnections", "Connections", "sessions", points(from, to, (_, i) => Math.round(24 + wave(i, 5) * 8))),
      ],
      episodes,
      summary: "In the period shown: CPU credits ran out once.",
      notes: [...base.notes, "Performance Insights is off, so DB load isn't measured; turning it on adds the DBLoad metric."],
    };
  }
  if (connection.startsWith("mssql")) {
    const pct = (seed: number, level: number, spread: number) => points(from, to, (_, i) => Math.max(0, Math.min(100, level + wave(i, seed) * spread)));
    return {
      ...base,
      provider: "azure",
      service: "Azure SQL Database",
      resource: "demo-sql/culprit_demo",
      tier: "GP_Gen5_2 · GeneralPurpose",
      series: [
        series("cpu_percent", "CPU", "percent", pct(1, 38, 24), { limit: 100, limit_label: "tier limit" }),
        series("physical_data_read_percent", "Data IO", "percent", pct(2, 12, 10), { limit: 100, limit_label: "tier limit" }),
        series(
          "log_write_percent",
          "Log writes",
          "percent",
          points(from, to, (t, i) => (inside(t) ? 99 + wave(i, 3) : 30 + wave(i, 3) * 20)),
          { limit: 100, limit_label: "tier limit" },
        ),
        series("workers_percent", "Workers", "percent", pct(4, 9, 6), { limit: 100, limit_label: "tier limit" }),
      ],
      episodes: [
        {
          kind: "log-rate",
          start: iso(Math.ceil(a / STEP) * STEP),
          end: iso(Math.ceil(b / STEP) * STEP),
          metric: "log_write_percent",
          worst: 100,
          title: "Log writes hit the tier's limit",
          detail: "Log writes stayed at 100% of GP_Gen5_2's limit: Azure held work back (commits wait on LOG_RATE_GOVERNOR).",
          ongoing: false,
        },
      ],
      summary: "In the period shown: log writes hit the tier's limit once.",
      notes: ["From Azure Monitor, one point per 5 min."],
    };
  }
  return null;
}
