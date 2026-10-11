//! Find Azure SQL databases the signed-in Azure CLI account can see, through Azure Resource Graph.
//! Only resource metadata is read; connections are saved with Entra ID auth, so no secret is involved.

use culprit_core::{Engine, Environment, Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use ts_rs::TS;

const RESOURCE_GRAPH: &str =
    "https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01";
const MAX_RESULTS: u32 = 500;

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct DiscoveredDatabase {
    pub alias: String,
    pub engine: Engine,
    pub environment: Environment,
    pub server: String,
    pub host: String,
    pub database: String,
    pub location: String,
    pub subscription: String,
    pub resource_group: String,
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
                engine: Engine::Mssql,
                environment: infer_environment(&format!("{} {}", r.database, r.server)),
                server: r.server,
                host,
                database: r.database,
                location: r.location,
                subscription: r.subscription,
                resource_group: r.resource_group,
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

/// `shop-alp-db` becomes `shop-alpha-db`; a database that also lives on a failover server gets `-failover`.
fn alias_for(d: &DiscoveredDatabase) -> String {
    let base: Vec<String> = tokens(&d.database)
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
    for i in 0..list.len() {
        let clash = list.iter().enumerate().any(|(j, o)| j != i && o.alias == list[i].alias);
        if clash {
            let server: String = tokens(&list[i].server).join("-");
            list[i].alias = format!("{}-{}", list[i].alias, server).chars().take(64).collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(server: &str, database: &str) -> DiscoveredDatabase {
        DiscoveredDatabase {
            alias: String::new(),
            engine: Engine::Mssql,
            environment: infer_environment(&format!("{database} {server}")),
            server: server.into(),
            host: format!("{server}.database.windows.net"),
            database: database.into(),
            location: "westeurope".into(),
            subscription: "s".into(),
            resource_group: "rg".into(),
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

    #[test]
    fn terms_are_restricted() {
        assert!(validate_terms(&["shop".into(), "alp".into()]).is_ok());
        assert!(validate_terms(&["shop' | project secrets".into()]).is_err());
    }
}
