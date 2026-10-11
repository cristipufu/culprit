//! Exercises the provider's interactive calls against `CULPRIT_MSSQL_ADO`. Development aid.
//!
//! cargo run -p culprit-mssql --example dev -- probe
//! cargo run -p culprit-mssql --example dev -- run "SELECT TOP 5 name FROM sys.tables"
//! cargo run -p culprit-mssql --example dev -- explain "SELECT …"
//! cargo run -p culprit-mssql --example dev -- analyze "SELECT …"
//! cargo run -p culprit-mssql --example dev -- live

use culprit_core::{ExplainMode, Provider};
use culprit_mssql::{MssqlProvider, parse_ado};

#[tokio::main]
async fn main() {
    let ado = std::env::var("CULPRIT_MSSQL_ADO").expect("set CULPRIT_MSSQL_ADO");
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "probe".into());
    let sql = args.next().unwrap_or_default();
    let provider = MssqlProvider::new(&parse_ado(&ado).expect("valid ADO string")).expect("valid spec");
    let output = match command.as_str() {
        "probe" => provider.probe().await.map(|v| serde_json::to_value(v).unwrap()),
        "run" => provider.run(&sql, 200).await.map(|v| serde_json::to_value(v).unwrap()),
        "explain" => provider.explain(&sql, ExplainMode::Estimated).await.map(|mut p| {
            p.raw = None;
            serde_json::to_value(p).unwrap()
        }),
        "analyze" => provider.explain(&sql, ExplainMode::Analyze).await.map(|mut p| {
            p.raw = None;
            serde_json::to_value(p).unwrap()
        }),
        "live" => provider.live().await.map(|v| serde_json::to_value(v).unwrap()),
        other => panic!("unknown command {other}"),
    };
    match output {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap()),
        Err(err) => {
            eprintln!(
                "{}: {} | {}",
                serde_json::to_string(&err.code).unwrap_or_default(),
                err.message,
                err.hint.unwrap_or_default()
            );
            std::process::exit(1);
        }
    }
}
