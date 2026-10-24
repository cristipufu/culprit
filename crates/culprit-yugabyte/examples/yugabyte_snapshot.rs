//! Disposable dev/yugabyte verification; the provider itself never reads process environment credentials.
use culprit_core::{ConnectionSpec, Engine, Environment, ExplainMode, Provider, SnapshotOptions};
use culprit_yugabyte::YugabyteProvider;

#[tokio::main]
async fn main() {
    let mut spec = ConnectionSpec {
        engine: Some(Engine::Yugabyte),
        host: "127.0.0.1".into(),
        port: Some(std::env::var("CULPRIT_YSQL_TEST_PORT").expect("set CULPRIT_YSQL_TEST_PORT").parse().unwrap()),
        database: Some("culprit_yugabyte".into()),
        user: Some("yugabyte".into()),
        environment: Environment::Local,
        ..Default::default()
    };
    spec.options.insert("sslmode".into(), "disable".into());
    let provider = YugabyteProvider::new(&spec).unwrap();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("probe") => provider.probe().await.map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("live") => provider.live().await.map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("analyze") => provider
            .explain(args.get(1).expect("SQL"), ExplainMode::Analyze)
            .await
            .map(|v| serde_json::to_string_pretty(&v).unwrap()),
        _ => provider
            .snapshot("yugabyte-demo", &SnapshotOptions::default())
            .await
            .map(|v| serde_json::to_string_pretty(&v).unwrap()),
    };
    match result {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("{}", serde_json::to_string(&e).unwrap());
            std::process::exit(1)
        }
    }
}
