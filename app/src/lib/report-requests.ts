import type { Report } from "../gen/Report";
import { errorMessage, type Api, type ApiError } from "./api";
import { isActionable } from "./findings";
import { LatestRequest } from "./requests";

export type ReportState =
  | { status: "none" }
  | { status: "loading" }
  | { status: "ready"; report: Report; scanning: boolean; scanError: ApiError | null }
  | { status: "error"; error: ApiError };

type Context = { active: string | null; windowMinutes: number; report: ReportState; finding: string | null };
type Update = { report: ReportState; finding?: string | null };
type Client = Pick<Api, "latestReport" | "scan">;
type Request = { name: string; windowMinutes: number; current: () => boolean };

/** Own the complete cache → scan lifecycle. Target identity alone cannot distinguish A → B → A or two scans of A. */
export class ReportRequests {
  private readonly owner = new LatestRequest();

  constructor(private readonly options: {
    client: () => Promise<Client>;
    read: () => Context;
    publish: (update: Update) => void;
    collected: () => Promise<unknown>;
  }) {}

  invalidate(): void {
    this.owner.invalidate();
  }

  private begin(name: string, windowMinutes: number): Request | null {
    const matches = () => this.options.read().active === name && this.options.read().windowMinutes === windowMinutes;
    if (!matches()) return null;
    const owns = this.owner.start();
    return { name, windowMinutes, current: () => owns() && matches() };
  }

  async open(name: string, windowMinutes: number, rescan: boolean): Promise<void> {
    const request = this.begin(name, windowMinutes);
    if (!request) return;
    try {
      const client = await this.options.client();
      if (!request.current()) return;
      const cached = await client.latestReport(name, windowMinutes);
      if (!request.current()) return;
      if (cached) this.accept(cached, rescan);
      if (!cached || rescan) await this.collect(client, request);
    } catch (error) {
      this.failed(request, error);
    }
  }

  async scan(name: string, windowMinutes: number): Promise<void> {
    const request = this.begin(name, windowMinutes);
    if (!request) return;
    this.scanning();
    try {
      const client = await this.options.client();
      if (request.current()) await this.collect(client, request);
    } catch (error) {
      this.failed(request, error);
    }
  }

  private scanning(): void {
    const current = this.options.read().report;
    if (current.status === "loading" || (current.status === "ready" && current.scanning && !current.scanError)) return;
    this.options.publish({ report: current.status === "ready"
      ? { ...current, scanning: true, scanError: null } : { status: "loading" } });
  }

  private accept(report: Report, scanning: boolean): void {
    const selected = this.options.read().finding;
    const finding = selected && report.findings.some((f) => f.id === selected)
      ? selected : report.findings.find(isActionable)?.id ?? null;
    this.options.publish({ report: { status: "ready", report, scanning, scanError: null }, finding });
  }

  private async collect(client: Client, request: Request): Promise<void> {
    if (!request.current()) return;
    this.scanning();
    if (!request.current()) return;
    const report = await client.scan(request.name, request.windowMinutes);
    if (!request.current()) return;
    this.accept(report, false);
    // Sidebar metadata has its own request owner; a refresh failure cannot invalidate a successful scan.
    void Promise.resolve().then(() => this.options.collected()).catch(() => {});
  }

  private failed(request: Request, error: unknown): void {
    if (!request.current()) return;
    const previous = this.options.read().report;
    const report: ReportState = previous.status === "ready"
      ? { ...previous, scanning: false, scanError: errorMessage(error) }
      : { status: "error", error: errorMessage(error) };
    this.options.publish({ report });
  }
}
