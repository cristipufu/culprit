import "../dashboard.css";

const TILES = ["db-time", "findings", "top-queries", "waits", "now", "health", "deadlocks", "indexes", "plans", "statistics"];

/** The dashboard's grid while the scan or the view loads: the same tiles in the same places, so nothing jumps. */
export function DashboardSkeleton() {
  return (
    <div className="dash" aria-busy="true" aria-label="Loading the dashboard">
      <header className="dash-head">
        <div className="dash-headline">
          <span className="skeleton" style={{ height: 22, width: "52%" }} />
        </div>
        <div className="dash-sub">
          <span className="skeleton" style={{ height: 12, width: 220 }} />
        </div>
      </header>
      <div className="dash-grid">
        {TILES.map((key, i) => (
          <section key={key} className={`dash-tile t-${key} loading`} aria-hidden>
            <span className="skeleton" style={{ height: 10, width: 90 }} />
            <span className="skeleton" style={{ height: 14, width: "88%", marginTop: 12 }} />
            <span className="skeleton" style={{ height: 14, width: "64%", marginTop: 6 }} />
            <span className="skeleton dash-sk-viz" style={{ opacity: 1 - i * 0.05 }} />
          </section>
        ))}
      </div>
    </div>
  );
}
