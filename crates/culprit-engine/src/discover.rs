//! Find databases the signed-in cloud CLI can see: Azure SQL through Azure Resource Graph, Amazon RDS and Aurora through
//! `aws rds describe-*`. Only resource metadata is read; nothing secret is involved.

use std::collections::BTreeMap;

use culprit_core::{AuthKind, Engine, Environment, Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use ts_rs::TS;

use crate::aws;

const RESOURCE_GRAPH: &str =
    "https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01";
const MAX_RESULTS: u32 = 500;

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
    /// Azure SQL server name, or RDS instance / Aurora cluster identifier.
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

#[derive(Deserialize)]
struct GraphResponse {
    data: Vec<Row>,
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

pub async fn azure_sql(terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
    validate_terms(terms)?;
    let body = serde_json::json!({ "query": query(terms), "options": { "$top": MAX_RESULTS } }).to_string();
    let output = Command::new(if cfg!(windows) { "az.cmd" } else { "az" })
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
    let response: GraphResponse = serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::internal(format!("Unexpected Resource Graph response: {e}")))?;
    let mut out: Vec<DiscoveredDatabase> = response
        .data
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
    let name = match d.cloud {
        Cloud::Azure => &d.database,
        Cloud::Aws => &d.server,
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
