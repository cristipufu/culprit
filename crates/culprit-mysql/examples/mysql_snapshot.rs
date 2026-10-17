//! Print a snapshot of a MySQL database as JSON.
//!
//! CULPRIT_MYSQL_URL='mysql://root:…@127.0.0.1:33306/shop' cargo run -p culprit-mysql --example mysql_snapshot

use culprit_core::{AuthKind, ConnectionSpec, Engine, Environment, Provider, SnapshotOptions};
use culprit_mysql::MysqlProvider;
use mysql_async::Opts;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("CULPRIT_MYSQL_URL").map_err(|_| "set CULPRIT_MYSQL_URL")?;
    let opts = Opts::from_url(&url)?;
    let spec = ConnectionSpec {
        engine: Some(Engine::Mysql),
        host: opts.ip_or_hostname().to_string(),
        port: Some(opts.tcp_port()),
        database: opts.db_name().map(str::to_string),
        user: opts.user().map(str::to_string),
        auth: if opts.pass().is_some() { AuthKind::Password } else { AuthKind::None },
        environment: Environment::Local,
        password: opts.pass().map(str::to_string),
        ..Default::default()
    };
    let provider = MysqlProvider::new(&spec)?;
    let snapshot = provider.snapshot("mysql-demo", &SnapshotOptions::default()).await?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}
