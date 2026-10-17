//! Prints a snapshot of the database named by `CULPRIT_MSSQL_ADO` (an ADO.NET connection string) as JSON.
//!
//! CULPRIT_MSSQL_ADO='Server=127.0.0.1,14333;Database=culprit_demo;User ID=sa;Password=…;Encrypt=false;TrustServerCertificate=true' \
//!   cargo run -p culprit-mssql --example mssql_snapshot -- [connection-name] [environment] > snapshot.json

use culprit_core::{Environment, Provider, SnapshotOptions};
use culprit_mssql::{MssqlProvider, parse_ado};

#[tokio::main]
async fn main() {
    let Ok(ado) = std::env::var("CULPRIT_MSSQL_ADO") else {
        eprintln!("Set CULPRIT_MSSQL_ADO to an ADO.NET connection string");
        std::process::exit(2);
    };
    let mut args = std::env::args().skip(1);
    let connection = args.next().unwrap_or_else(|| "example".into());
    let environment = match args.next().as_deref() {
        Some("production") => Environment::Production,
        Some("staging") => Environment::Staging,
        Some("local") => Environment::Local,
        _ => Environment::Development,
    };
    let result = async {
        let mut spec = parse_ado(&ado)?;
        spec.environment = environment;
        let provider = MssqlProvider::new(&spec)?;
        provider.snapshot(&connection, &SnapshotOptions::default()).await
    }
    .await;
    match result {
        Ok(snapshot) => println!("{}", serde_json::to_string_pretty(&snapshot).expect("snapshot serializes")),
        Err(err) => {
            eprintln!("{}: {}", serde_json::to_string(&err.code).unwrap_or_default(), err.message);
            if let Some(hint) = err.hint {
                eprintln!("hint: {hint}");
            }
            std::process::exit(1);
        }
    }
}
