//! Find databases the signed-in cloud CLI can see: Azure SQL and Azure Database for PostgreSQL / MySQL flexible servers
//! through Azure Resource Graph and Resource Manager, Amazon RDS and Aurora through `aws rds describe-*`. Only resource
//! metadata is read; nothing secret is involved.

use std::collections::BTreeMap;
use std::sync::Arc;

use culprit_core::{AuthKind, Engine, Environment, Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use ts_rs::TS;

use crate::{aws, http};

const RESOURCE_GRAPH: &str =
    "https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01";
const MAX_RESULTS: u32 = 500;
const MANAGEMENT: &str = "https://management.azure.com";
/// Database listings per flexible server run in parallel, this many at a time.
const LISTING_CONCURRENCY: usize = 16;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Cloud {
    #[default]
    Azure,
    Aws,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct DiscoveredDatabase {
    pub alias: String,
    #[serde(default)]
    pub cloud: Cloud,
    pub engine: Engine,
    pub environment: Environment,
    /// Azure SQL or flexible server name, or RDS instance / Aurora cluster identifier.
    pub server: String,
    pub host: String,
    /// Empty when the engine connects without a default database (MySQL).
    pub database: String,
    pub location: String,
    /// Azure subscription id or AWS account id.
    pub subscription: String,
    pub resource_group: String,
    #[serde(default)]
    pub port: Option<u16>,
    /// The master user name (RDS); not a secret.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// How the saved connection will sign in: Entra ID (Azure), AWS IAM, or a password set once.
    #[serde(default)]
    pub auth: AuthKind,
    /// The saved connection that already points at this database, if any.
    pub saved_as: Option<String>,
}

#[derive(Deserialize)]
struct Row {
    server: String,
    database: String,
    location: String,
    #[serde(rename = "resourceGroup")]
    resource_group: String,
    #[serde(rename = "subscriptionId")]
    subscription: String,
    fqdn: Option<String>,
}

/// Every term must appear in the database or server name. Terms are restricted to identifier characters because they are embedded in a KQL query.
pub fn validate_terms(terms: &[String]) -> Result<()> {
    let ok = terms.iter().all(|t| {
        !t.is_empty() && t.len() <= 64 && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    });
    if ok {
        return Ok(());
    }
    Err(Error::invalid("Search terms may only contain letters, digits, '-', '_' and '.'"))
}

fn query(terms: &[String]) -> String {
    let filters: String =
        terms.iter().map(|t| format!("| where name contains '{t}' or server contains '{t}' ")).collect();
    format!(
        "resources \
         | where type =~ 'microsoft.sql/servers/databases' and name !~ 'master' \
         | extend server = tostring(split(id, '/')[8]) \
         {filters}\
         | join kind=leftouter (resources | where type =~ 'microsoft.sql/servers' \
             | project server = name, fqdn = tostring(properties.fullyQualifiedDomainName)) on server \
         | project server, database = name, location, resourceGroup, subscriptionId, fqdn \
         | order by database asc, server asc"
    )
}

/// Run a Resource Graph query through the Azure CLI's signed-in account.
pub(crate) async fn resource_graph<T: serde::de::DeserializeOwned>(query: &str) -> Result<Vec<T>> {
    #[derive(Deserialize)]
    struct Response<T> {
        data: Vec<T>,
    }
    let body = serde_json::json!({ "query": query, "options": { "$top": MAX_RESULTS } }).to_string();
    let output = Command::new(az())
        .args(["rest", "--method", "post", "--url", RESOURCE_GRAPH, "--body", &body, "--output", "json"])
        .output()
        .await
        .map_err(|e| {
            Error::new(ErrorCode::AuthFailed, format!("Could not run the Azure CLI: {e}"))
                .with_hint("Install the Azure CLI and run `az login`")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(
            ErrorCode::QueryFailed,
            format!("Azure Resource Graph query failed: {}", stderr.trim()),
        )
        .with_hint("Run `az login` and retry"));
    }
    let response: Response<T> = serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::internal(format!("Unexpected Resource Graph response: {e}")))?;
    Ok(response.data)
}

pub(crate) fn az() -> &'static str {
    if cfg!(windows) { "az.cmd" } else { "az" }
}

/// Azure SQL databases and PostgreSQL / MySQL flexible-server databases, in one list.
pub async fn azure(terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
    validate_terms(terms)?;
    let (sql, flexible) = tokio::join!(azure_sql(terms), azure_flexible(terms));
    let mut out = match (sql, flexible) {
        (Ok(mut a), Ok(b)) => {
            a.extend(b);
            a
        }
        (Ok(a), Err(_)) => a,
        (Err(_), Ok(b)) => b,
        (Err(e), Err(_)) => return Err(e),
    };
    out.sort_by(|a, b| a.database.cmp(&b.database).then(a.server.cmp(&b.server)));
    assign_aliases(&mut out);
    Ok(out)
}

pub async fn azure_sql(terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
    validate_terms(terms)?;
    let rows: Vec<Row> = resource_graph(&query(terms)).await?;
    let mut out: Vec<DiscoveredDatabase> = rows
        .into_iter()
        .map(|r| {
            let host = r.fqdn.clone().unwrap_or_else(|| format!("{}.database.windows.net", r.server));
            DiscoveredDatabase {
                alias: String::new(),
                cloud: Cloud::Azure,
                engine: Engine::Mssql,
                environment: infer_environment(&format!("{} {}", r.database, r.server)),
                server: r.server,
                host,
                database: r.database,
                location: r.location,
                subscription: r.subscription,
                resource_group: r.resource_group,
                port: Some(1433),
                user: None,
                region: None,
                auth: AuthKind::Entra,
                saved_as: None,
            }
        })
        .filter(|d: &DiscoveredDatabase| {
            terms.iter().all(|t| matches_term(&d.database, t) || matches_term(&d.server, t))
        })
        .collect();
    assign_aliases(&mut out);
    Ok(out)
}

#[derive(Deserialize)]
struct FlexibleServer {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    location: String,
    #[serde(rename = "resourceGroup")]
    resource_group: String,
    #[serde(rename = "subscriptionId")]
    subscription: String,
    fqdn: Option<String>,
    /// PostgreSQL: `Enabled` when Microsoft Entra ID sign-in is on.
    entra: Option<String>,
    admin: Option<String>,
    state: Option<String>,
}

const FLEXIBLE_QUERY: &str = "resources \
     | where type in~ ('microsoft.dbforpostgresql/flexibleservers', 'microsoft.dbformysql/flexibleservers') \
     | project id, name, type, location, resourceGroup, subscriptionId, \
         fqdn = tostring(properties.fullyQualifiedDomainName), \
         entra = tostring(properties.authConfig.activeDirectoryAuth), \
         admin = tostring(properties.administratorLogin), state = tostring(properties.state) \
     | order by name asc";

/// Databases that come with every server and hold nothing of the application's.
fn system_database(engine: Engine, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match engine {
        Engine::Postgres => matches!(name.as_str(), "azure_maintenance" | "azure_sys" | "template0" | "template1"),
        Engine::Mysql => matches!(name.as_str(), "information_schema" | "mysql" | "performance_schema" | "sys"),
        Engine::Mssql => name == "master",
        Engine::Sqlite => false,
    }
}

/// Resource ids from Resource Graph become Resource Manager URLs; anything but a plain flexible-server id is refused.
fn flexible_server_url(id: &str, child: &str, engine: Engine) -> Option<String> {
    let provider = match engine {
        Engine::Postgres => "/providers/microsoft.dbforpostgresql/flexibleservers/",
        Engine::Mysql => "/providers/microsoft.dbformysql/flexibleservers/",
        Engine::Mssql | Engine::Sqlite => return None,
    };
    let version = if engine == Engine::Postgres { "2024-08-01" } else { "2023-12-30" };
    let plain = id.starts_with("/subscriptions/")
        && !id.contains("..")
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '(' | ')'))
        && id.to_ascii_lowercase().contains(provider);
    plain.then(|| format!("{MANAGEMENT}{id}/{child}?api-version={version}"))
}

/// A Resource Manager token from the Azure CLI. Only ever sent to management.azure.com.
async fn management_token() -> Result<String> {
    let output = Command::new(az())
        .args(["account", "get-access-token", "--resource", "https://management.azure.com/", "--query", "accessToken"])
        .args(["--output", "tsv"])
        .output()
        .await
        .map_err(|e| Error::new(ErrorCode::AuthFailed, format!("Could not run the Azure CLI: {e}")))?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        return Err(Error::new(ErrorCode::AuthFailed, "The Azure CLI could not issue a Resource Manager token"));
    }
    Ok(token)
}

/// Names under a flexible server's `databases` or `administrators`; `None` when they can't be listed.
async fn list_children(url: String, token: String) -> Option<Vec<String>> {
    let reply = http::get_bearer(url, token, "Listing Azure databases").await.ok()?;
    if reply.status != 200 {
        return None;
    }
    let names = reply.json.get("value")?.as_array()?.iter().filter_map(|v| v.get("name")?.as_str().map(str::to_string));
    Some(names.collect())
}

/// Azure Database for PostgreSQL and MySQL flexible servers, one row per application database. Servers whose databases
/// can't be listed (stopped, no permission) get one row for the server.
pub async fn azure_flexible(terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
    validate_terms(terms)?;
    let servers: Vec<FlexibleServer> = resource_graph(FLEXIBLE_QUERY).await?;
    if servers.is_empty() {
        return Ok(Vec::new());
    }
    let token = management_token().await.ok();
    let gate = Arc::new(tokio::sync::Semaphore::new(LISTING_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, server) in servers.iter().enumerate() {
        let engine = flexible_engine(&server.kind);
        let ready = server.state.as_deref().is_none_or(|s| s.eq_ignore_ascii_case("ready"));
        let (Some(token), Some(engine), true) = (token.clone(), engine, ready) else { continue };
        let Some(databases) = flexible_server_url(&server.id, "databases", engine) else { continue };
        let admins =
            (engine == Engine::Mysql).then(|| flexible_server_url(&server.id, "administrators", engine)).flatten();
        let gate = gate.clone();
        tasks.spawn(async move {
            let _permit = gate.acquire_owned().await.ok()?;
            let names = list_children(databases, token.clone()).await;
            let entra = match admins {
                Some(url) => Some(list_children(url, token).await.is_some_and(|a| !a.is_empty())),
                None => None,
            };
            Some((i, names, entra))
        });
    }
    let mut listed: BTreeMap<usize, (Option<Vec<String>>, Option<bool>)> = BTreeMap::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok(Some((i, names, entra))) = joined {
            listed.insert(i, (names, entra));
        }
    }
    let mut out = Vec::new();
    for (i, server) in servers.into_iter().enumerate() {
        let Some(engine) = flexible_engine(&server.kind) else { continue };
        let (names, mysql_entra) = listed.remove(&i).unwrap_or((None, None));
        let entra = match engine {
            Engine::Postgres => server.entra.as_deref().is_some_and(|e| e.eq_ignore_ascii_case("enabled")),
            _ => mysql_entra.unwrap_or(false),
        };
        let suffix =
            if engine == Engine::Postgres { "postgres.database.azure.com" } else { "mysql.database.azure.com" };
        let host = server.fqdn.clone().filter(|h| !h.is_empty()).unwrap_or_else(|| format!("{}.{suffix}", server.name));
        let mut databases: Vec<String> =
            names.unwrap_or_default().into_iter().filter(|d| !system_database(engine, d)).collect();
        if databases.len() > 1 {
            databases.retain(|d| d != "postgres");
        }
        if databases.is_empty() {
            databases.push(default_database(engine).to_string());
        }
        for database in databases {
            out.push(DiscoveredDatabase {
                alias: String::new(),
                cloud: Cloud::Azure,
                engine,
                environment: infer_environment(&format!("{database} {}", server.name)),
                server: server.name.clone(),
                host: host.clone(),
                database,
                location: server.location.clone(),
                subscription: server.subscription.clone(),
                resource_group: server.resource_group.clone(),
                port: Some(if engine == Engine::Postgres { 5432 } else { 3306 }),
                user: if entra { None } else { server.admin.clone().filter(|a| !a.is_empty()) },
                region: None,
                auth: if entra { AuthKind::Entra } else { AuthKind::Password },
                saved_as: None,
            });
        }
    }
    out.retain(|d| terms.iter().all(|t| matches_term(&d.database, t) || matches_term(&d.server, t)));
    assign_aliases(&mut out);
    Ok(out)
}

fn flexible_engine(kind: &str) -> Option<Engine> {
    match kind.to_ascii_lowercase().as_str() {
        "microsoft.dbforpostgresql/flexibleservers" => Some(Engine::Postgres),
        "microsoft.dbformysql/flexibleservers" => Some(Engine::Mysql),
        _ => None,
    }
}

/// `shop` matches `shop-alp-db` but not `tenant_t_kjrshopbj3g236fv8_db`: plain terms match whole name segments, terms with separators match anywhere.
fn matches_term(name: &str, term: &str) -> bool {
    let term = term.to_ascii_lowercase();
    if term.contains(|c: char| !c.is_ascii_alphanumeric()) {
        return name.to_ascii_lowercase().contains(&term);
    }
    tokens(name).contains(&term)
}

fn tokens(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn infer_environment(text: &str) -> Environment {
    let t = tokens(text);
    let has = |words: &[&str]| t.iter().any(|x| words.contains(&x.as_str()));
    if has(&["prd", "prod", "production"]) {
        return Environment::Production;
    }
    if has(&["stg", "staging", "alp", "alpha", "uat", "preprod"]) {
        return Environment::Staging;
    }
    Environment::Development
}

/// `shop-alp-db` becomes `shop-alpha-db`; a database that also lives on a failover server gets `-failover`. On AWS the
/// instance or cluster identifier names the database better than its DBName, which is often just `postgres`.
fn alias_for(d: &DiscoveredDatabase) -> String {
    let default = d.database.is_empty() || d.database == default_database(d.engine);
    let name = match d.cloud {
        Cloud::Azure if !default => &d.database,
        _ => &d.server,
    };
    let base: Vec<String> = tokens(name)
        .into_iter()
        .map(|t| match t.as_str() {
            "alp" => "alpha".to_string(),
            "prd" => "prod".to_string(),
            _ => t,
        })
        .collect();
    let mut alias = base.join("-");
    if d.server.to_ascii_lowercase().contains("failover") {
        alias.push_str("-failover");
    }
    alias.chars().take(64).collect()
}

fn assign_aliases(list: &mut [DiscoveredDatabase]) {
    for d in list.iter_mut() {
        d.alias = alias_for(d);
    }
    let clashes: Vec<bool> =
        (0..list.len()).map(|i| list.iter().enumerate().any(|(j, o)| j != i && o.alias == list[i].alias)).collect();
    for (d, clash) in list.iter_mut().zip(clashes) {
        if !clash {
            continue;
        }
        let suffix = match (d.cloud, d.region.as_deref()) {
            (Cloud::Aws, Some(region)) => aws::region_short(region),
            _ => tokens(&d.server).join("-"),
        };
        d.alias = format!("{}-{}", d.alias, suffix).chars().take(64).collect();
    }
}

/// RDS instances and Aurora clusters the signed-in AWS CLI can see in `regions` (all enabled regions when empty).
pub async fn aws_rds(regions: &[String], terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
    validate_terms(terms)?;
    if let Some(bad) = regions.iter().find(|r| !aws::valid_region(r)) {
        return Err(Error::invalid(format!("'{bad}' is not an AWS region"))
            .with_hint("Use region codes like us-east-1 or eu-central-1"));
    }
    let identity: serde_json::Value = parse(&aws::run(&["sts", "get-caller-identity", "--output", "json"]).await?)?;
    let account = identity.get("Account").and_then(|a| a.as_str()).unwrap_or_default().to_string();
    let regions = if regions.is_empty() { enabled_regions().await? } else { regions.to_vec() };

    let mut tasks = tokio::task::JoinSet::new();
    for region in regions {
        tasks.spawn(async move {
            let instances = aws::run(&["rds", "describe-db-instances", "--region", &region, "--output", "json"]).await;
            let clusters = aws::run(&["rds", "describe-db-clusters", "--region", &region, "--output", "json"]).await;
            (region, instances, clusters)
        });
    }
    let mut out = Vec::new();
    let mut failures: Vec<Error> = Vec::new();
    let mut succeeded = 0;
    while let Some(joined) = tasks.join_next().await {
        let (region, instances, clusters) = joined.map_err(|e| Error::internal(e.to_string()))?;
        match (instances, clusters) {
            (Ok(instances), Ok(clusters)) => {
                succeeded += 1;
                out.extend(rds_rows(&region, &account, &parse(&instances)?, &parse(&clusters)?));
            }
            (Err(e), _) | (_, Err(e)) => failures.push(e),
        }
    }
    if let Some(e) = failures.iter().find(|e| e.code == ErrorCode::AuthFailed) {
        return Err(e.clone());
    }
    if succeeded == 0
        && let Some(e) = failures.into_iter().next()
    {
        return Err(e);
    }
    out.retain(|d| terms.iter().all(|t| matches_term(&d.server, t) || matches_term(&d.database, t)));
    out.sort_by(|a, b| a.server.cmp(&b.server).then(a.location.cmp(&b.location)).then(a.host.cmp(&b.host)));
    assign_aliases(&mut out);
    Ok(out)
}

fn parse(bytes: &[u8]) -> Result<serde_json::Value> {
    serde_json::from_slice(bytes).map_err(|e| Error::internal(format!("Unexpected AWS CLI output: {e}")))
}

async fn enabled_regions() -> Result<Vec<String>> {
    let listed = aws::run(&[
        "account",
        "list-regions",
        "--region-opt-status-contains",
        "ENABLED",
        "ENABLED_BY_DEFAULT",
        "--region",
        "us-east-1",
        "--output",
        "json",
    ])
    .await;
    let value = match listed {
        Ok(bytes) => parse(&bytes)?,
        Err(e) if e.code == ErrorCode::AuthFailed => return Err(e),
        Err(_) => parse(&aws::run(&["ec2", "describe-regions", "--region", "us-east-1", "--output", "json"]).await?)?,
    };
    let regions: Vec<String> = value
        .get("Regions")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("RegionName").and_then(|n| n.as_str()).map(str::to_string))
        .filter(|r| aws::valid_region(r))
        .collect();
    if regions.is_empty() {
        return Err(Error::internal("The AWS CLI listed no enabled regions").with_hint("Pass the regions explicitly"));
    }
    Ok(regions)
}

fn rds_engine(engine: &str) -> Option<Engine> {
    let engine = engine.to_ascii_lowercase();
    match engine.as_str() {
        "postgres" | "aurora-postgresql" => Some(Engine::Postgres),
        "mysql" | "mariadb" | "aurora-mysql" | "aurora" => Some(Engine::Mysql),
        e if e.starts_with("sqlserver") => Some(Engine::Mssql),
        _ => None,
    }
}

fn tags(resource: &serde_json::Value) -> BTreeMap<String, String> {
    resource
        .get("TagList")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t.get("Key")?.as_str()?.to_ascii_lowercase(), t.get("Value")?.as_str()?.to_string())))
        .collect()
}

/// An environment tag (`Environment`, `env`, `stage`) wins over what the names suggest.
fn rds_environment(names: &str, tags: &BTreeMap<String, String>) -> Environment {
    let tagged = ["environment", "env", "stage"].iter().find_map(|k| tags.get(*k));
    match tagged {
        Some(value) => match infer_environment(value) {
            Environment::Development
                if !matches!(value.to_ascii_lowercase().as_str(), "dev" | "development" | "test") =>
            {
                infer_environment(names)
            }
            environment => environment,
        },
        None => infer_environment(names),
    }
}

fn str_of<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

fn default_database(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::Mssql => "master",
        Engine::Mysql => "",
        Engine::Sqlite => "main",
    }
}

/// Rows for one region's `describe-db-instances` and `describe-db-clusters` output. Aurora members are reached through their
/// cluster's writer and reader endpoints rather than one by one.
fn rds_rows(
    region: &str,
    account: &str,
    instances: &serde_json::Value,
    clusters: &serde_json::Value,
) -> Vec<DiscoveredDatabase> {
    let mut out = Vec::new();
    let row = |server: &str,
               engine: Engine,
               host: &str,
               port: Option<u64>,
               db: Option<&str>,
               user: Option<&str>,
               iam: bool,
               environment: Environment| {
        DiscoveredDatabase {
            alias: String::new(),
            cloud: Cloud::Aws,
            engine,
            environment,
            server: server.to_string(),
            host: host.to_string(),
            database: db.unwrap_or(default_database(engine)).to_string(),
            location: region.to_string(),
            subscription: account.to_string(),
            resource_group: String::new(),
            port: port.and_then(|p| u16::try_from(p).ok()),
            user: user.map(str::to_string),
            region: Some(region.to_string()),
            auth: if iam && engine != Engine::Mssql { AuthKind::AwsIam } else { AuthKind::Password },
            saved_as: None,
        }
    };
    for i in instances.get("DBInstances").and_then(|x| x.as_array()).into_iter().flatten() {
        if str_of(i, "DBClusterIdentifier").is_some() {
            continue;
        }
        let (Some(id), Some(engine)) = (str_of(i, "DBInstanceIdentifier"), str_of(i, "Engine").and_then(rds_engine))
        else {
            continue;
        };
        let Some(endpoint) = i.get("Endpoint") else { continue };
        let Some(host) = str_of(endpoint, "Address") else { continue };
        let db = str_of(i, "DBName");
        let environment = rds_environment(&format!("{id} {}", db.unwrap_or_default()), &tags(i));
        let iam = i.get("IAMDatabaseAuthenticationEnabled").and_then(|b| b.as_bool()).unwrap_or(false);
        out.push(row(
            id,
            engine,
            host,
            endpoint.get("Port").and_then(|p| p.as_u64()),
            db,
            str_of(i, "MasterUsername"),
            iam,
            environment,
        ));
    }
    for c in clusters.get("DBClusters").and_then(|x| x.as_array()).into_iter().flatten() {
        let (Some(id), Some(engine)) = (str_of(c, "DBClusterIdentifier"), str_of(c, "Engine").and_then(rds_engine))
        else {
            continue;
        };
        let db = str_of(c, "DatabaseName");
        let environment = rds_environment(&format!("{id} {}", db.unwrap_or_default()), &tags(c));
        let iam = c.get("IAMDatabaseAuthenticationEnabled").and_then(|b| b.as_bool()).unwrap_or(false);
        let port = c.get("Port").and_then(|p| p.as_u64());
        let user = str_of(c, "MasterUsername");
        if let Some(host) = str_of(c, "Endpoint") {
            out.push(row(id, engine, host, port, db, user, iam, environment));
        }
        if let Some(host) = str_of(c, "ReaderEndpoint") {
            out.push(row(&format!("{id}-reader"), engine, host, port, db, user, iam, environment));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(server: &str, database: &str) -> DiscoveredDatabase {
        DiscoveredDatabase {
            alias: String::new(),
            cloud: Cloud::Azure,
            engine: Engine::Mssql,
            environment: infer_environment(&format!("{database} {server}")),
            server: server.into(),
            host: format!("{server}.database.windows.net"),
            database: database.into(),
            location: "westeurope".into(),
            subscription: "s".into(),
            resource_group: "rg".into(),
            port: Some(1433),
            user: None,
            region: None,
            auth: AuthKind::Entra,
            saved_as: None,
        }
    }

    #[test]
    fn aliases_and_environments() {
        let mut list = vec![
            db("sql-acme-alp", "shop-alp-db"),
            db("sql-acme-prd", "shop-prd-db"),
            db("sql-acme-prd-failover", "shop-prd-db"),
        ];
        assign_aliases(&mut list);
        assert_eq!(list[0].alias, "shop-alpha-db");
        assert_eq!(list[0].environment, Environment::Staging);
        assert_eq!(list[1].alias, "shop-prod-db");
        assert_eq!(list[1].environment, Environment::Production);
        assert_eq!(list[2].alias, "shop-prod-db-failover");
    }

    #[test]
    fn terms_match_whole_segments() {
        assert!(matches_term("shop-alp-db", "shop"));
        assert!(matches_term("shop-alp-db", "alp"));
        assert!(!matches_term("tenant_t_kjrshopbj3g236fv8_db", "shop"));
        assert!(matches_term("sql-acme-alp", "acme-alp"));
    }

    const INSTANCES: &str = r#"{"DBInstances": [
        {"DBInstanceIdentifier": "shop-prod-db", "Engine": "postgres", "DBName": "shop", "MasterUsername": "app",
         "Endpoint": {"Address": "shop-prod-db.abc123.us-east-1.rds.amazonaws.com", "Port": 5432},
         "IAMDatabaseAuthenticationEnabled": true, "TagList": []},
        {"DBInstanceIdentifier": "events", "Engine": "mysql", "MasterUsername": "admin",
         "Endpoint": {"Address": "events.abc123.us-east-1.rds.amazonaws.com", "Port": 3306},
         "IAMDatabaseAuthenticationEnabled": false, "TagList": [{"Key": "Environment", "Value": "staging"}]},
        {"DBInstanceIdentifier": "ledger", "Engine": "oracle-ee",
         "Endpoint": {"Address": "ledger.abc123.us-east-1.rds.amazonaws.com", "Port": 1521}},
        {"DBInstanceIdentifier": "orders-instance-1", "Engine": "aurora-postgresql", "DBClusterIdentifier": "orders",
         "Endpoint": {"Address": "orders-instance-1.abc123.us-east-1.rds.amazonaws.com", "Port": 5432}},
        {"DBInstanceIdentifier": "reports", "Engine": "sqlserver-se", "MasterUsername": "admin",
         "Endpoint": {"Address": "reports.abc123.us-east-1.rds.amazonaws.com", "Port": 1433},
         "IAMDatabaseAuthenticationEnabled": true}
    ]}"#;

    const CLUSTERS: &str = r#"{"DBClusters": [
        {"DBClusterIdentifier": "orders", "Engine": "aurora-postgresql", "DatabaseName": "orders", "MasterUsername": "app",
         "Endpoint": "orders.cluster-abc123.us-east-1.rds.amazonaws.com",
         "ReaderEndpoint": "orders.cluster-ro-abc123.us-east-1.rds.amazonaws.com", "Port": 5432,
         "IAMDatabaseAuthenticationEnabled": true, "TagList": [{"Key": "env", "Value": "prod"}]}
    ]}"#;

    fn rows(region: &str) -> Vec<DiscoveredDatabase> {
        rds_rows(
            region,
            "111122223333",
            &serde_json::from_str(INSTANCES).unwrap(),
            &serde_json::from_str(CLUSTERS).unwrap(),
        )
    }

    #[test]
    fn rds_instances_and_aurora_clusters_become_rows() {
        let list = rows("us-east-1");
        let names: Vec<&str> = list.iter().map(|d| d.server.as_str()).collect();
        assert_eq!(names, vec!["shop-prod-db", "events", "reports", "orders", "orders-reader"]);

        let shop = &list[0];
        assert_eq!((shop.engine, shop.auth, shop.port), (Engine::Postgres, AuthKind::AwsIam, Some(5432)));
        assert_eq!((shop.database.as_str(), shop.user.as_deref()), ("shop", Some("app")));
        assert_eq!(shop.environment, Environment::Production);
        assert_eq!(shop.region.as_deref(), Some("us-east-1"));
        assert_eq!(shop.subscription, "111122223333");

        let events = &list[1];
        assert_eq!((events.engine, events.auth, events.database.as_str()), (Engine::Mysql, AuthKind::Password, ""));
        assert_eq!(events.environment, Environment::Staging, "the Environment tag wins");

        let reports = &list[2];
        assert_eq!(
            (reports.engine, reports.auth, reports.database.as_str()),
            (Engine::Mssql, AuthKind::Password, "master")
        );

        let reader = &list[4];
        assert_eq!(reader.host, "orders.cluster-ro-abc123.us-east-1.rds.amazonaws.com");
        assert_eq!(reader.environment, Environment::Production, "the env tag says prod");
    }

    #[test]
    fn same_names_in_two_regions_get_region_suffixes() {
        let mut list: Vec<DiscoveredDatabase> = rows("us-east-1").into_iter().chain(rows("eu-central-1")).collect();
        assign_aliases(&mut list);
        let aliases: Vec<&str> = list.iter().map(|d| d.alias.as_str()).collect();
        assert!(aliases.contains(&"shop-prod-db-use1") && aliases.contains(&"shop-prod-db-euc1"), "{aliases:?}");
        assert!(aliases.contains(&"orders-reader-euc1"));
        let mut single = rows("us-east-1");
        assign_aliases(&mut single);
        assert_eq!(single[0].alias, "shop-prod-db");
    }

    #[tokio::test]
    async fn bad_regions_are_rejected_before_calling_aws() {
        let e = aws_rds(&["us-east-1 --profile x".into()], &[]).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn terms_are_restricted() {
        assert!(validate_terms(&["shop".into(), "alp".into()]).is_ok());
        assert!(validate_terms(&["shop' | project secrets".into()]).is_err());
    }
}
