//! Print a snapshot of a SQLite file as JSON, opened read-only.
//!
//! cargo run -p culprit-sqlite --example sqlite_snapshot -- path/to/app.db
//!
//! Other provider calls, for poking at a file by hand: `-- <path> probe`, `-- <path> explain <sql>`, `-- <path> run <sql>`.

use culprit_core::{ConnectionSpec, Engine, ExplainMode, Provider, SnapshotOptions};
use culprit_sqlite::SqliteProvider;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: snapshot <path> [probe | explain <sql> | run <sql>]")?;
    let spec = ConnectionSpec { engine: Some(Engine::Sqlite), database: Some(path), ..Default::default() };
    let provider = SqliteProvider::new(&spec)?;
    let command = args.next();
    let rest = args.collect::<Vec<_>>().join(" ");
    let json = match command.as_deref() {
        None => serde_json::to_string_pretty(&provider.snapshot("sqlite-demo", &SnapshotOptions::default()).await?)?,
        Some("probe") => serde_json::to_string_pretty(&provider.probe().await?)?,
        Some("explain") => serde_json::to_string_pretty(&provider.explain(&rest, ExplainMode::Estimated).await?)?,
        Some("run") => serde_json::to_string_pretty(&provider.run(&rest, 50).await?)?,
        Some(other) => return Err(format!("unknown command {other}").into()),
    };
    println!("{json}");
    Ok(())
}
