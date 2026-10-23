//! Print a snapshot of the database at `CULPRIT_PG_URL` as JSON.
//!
//! CULPRIT_PG_URL='postgres://user:pass@localhost:5432/db?sslmode=disable' cargo run -p culprit-postgres --example postgres_snapshot
//!
//! Other provider calls, for poking at a server by hand: `-- probe`, `-- live`, `-- explain <sql>`, `-- analyze <sql>`, `-- run <sql>`.

use std::str::FromStr;

use culprit_core::{ConnectionSpec, Engine, Environment, ExplainMode, Provider, SnapshotOptions};
use culprit_postgres::PostgresProvider;
use tokio_postgres::config::Host;

#[tokio::main]
async fn main() {
    let url = std::env::var("CULPRIT_PG_URL").expect("set CULPRIT_PG_URL to a postgres:// URL");
    // tokio-postgres only knows disable/prefer/require; Culprit handles the verify-* modes itself.
    let sslmode = url.split(['?', '&']).find_map(|p| p.strip_prefix("sslmode=")).unwrap_or("prefer").to_string();
    let without_sslmode = url.replace(&format!("sslmode={sslmode}"), "sslmode=prefer");
    let config =
        tokio_postgres::Config::from_str(&without_sslmode).expect("CULPRIT_PG_URL is not a valid Postgres URL");
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(host)) => host.clone(),
        _ => "localhost".to_string(),
    };
    let mut spec = ConnectionSpec {
        engine: Some(Engine::Postgres),
        host,
        port: config.get_ports().first().copied(),
        database: config.get_dbname().map(str::to_string),
        user: config.get_user().map(str::to_string),
        environment: Environment::Local,
        password: config.get_password().map(|p| String::from_utf8_lossy(p).into_owned()),
        ..Default::default()
    };
    spec.options.insert("sslmode".into(), sslmode);

    let provider = PostgresProvider::new(&spec).unwrap_or_else(|e| panic!("{e}"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sql = args.get(1).cloned().unwrap_or_default();
    let connection = std::env::var("CULPRIT_CONNECTION").unwrap_or_else(|_| "postgres-demo".into());
    let output = match args.first().map(String::as_str) {
        None | Some("snapshot") => provider
            .snapshot(&connection, &SnapshotOptions::default())
            .await
            .map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("probe") => provider.probe().await.map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("live") => provider.live().await.map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("deadlocks") => provider.deadlocks(24.0).await.map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("explain") => {
            provider.explain(&sql, ExplainMode::Estimated).await.map(|v| serde_json::to_string_pretty(&v).unwrap())
        }
        Some("analyze") => {
            provider.explain(&sql, ExplainMode::Analyze).await.map(|v| serde_json::to_string_pretty(&v).unwrap())
        }
        Some("run") => provider.run(&sql, 5).await.map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some(other) => panic!("unknown command `{other}`"),
    };
    match output {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("{}", serde_json::to_string(&e).unwrap());
            std::process::exit(1);
        }
    }
}
