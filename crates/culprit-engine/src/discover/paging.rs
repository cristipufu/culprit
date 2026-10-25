//! Inventory is consumed in bounded pages. Only matched rows survive the current provider page.
use std::{collections::BTreeSet, future::Future, time::Duration};

use serde_json::{Value, json};
use tokio::{sync::Semaphore, time::Instant};

use super::*;

const PAGE_SIZE: usize = 50;
const MAX_REQUESTS: usize = 256;
const MAX_REGIONS: usize = 100;
const MAX_CURSOR: usize = 16 * 1024;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
static SEARCHES: Semaphore = Semaphore::const_new(2);

trait Transport: Send + Sync {
    fn aws(&self, args: &[String]) -> impl Future<Output = Result<Value>> + Send;
    fn graph(&self, query: &str, token: Option<&str>) -> impl Future<Output = Result<Value>> + Send;
    fn token(&self) -> impl Future<Output = Result<String>> + Send;
    fn arm(&self, url: &str, token: &str) -> impl Future<Output = Result<Value>> + Send;
}

async fn az_output(args: &[&str]) -> Result<std::process::Output> {
    let mut command = crate::program::command(crate::program::Cli::Azure)?;
    command.args(args);
    crate::cloud_process::output(command).await.map_err(|error| {
        if error.code == ErrorCode::NotFound {
            Error::new(ErrorCode::AuthFailed, "The Azure CLI was not found in PATH or standard install locations")
                .with_hint("Install the Azure CLI and run `az login`")
        } else {
            error
        }
    })
}

struct Live;
impl Transport for Live {
    async fn aws(&self, args: &[String]) -> Result<Value> {
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        serde_json::from_slice(&aws::run(&args).await?)
            .map_err(|_| Error::new(ErrorCode::QueryFailed, "AWS returned invalid inventory JSON"))
    }

    async fn graph(&self, query: &str, token: Option<&str>) -> Result<Value> {
        let mut body = json!({"query": query, "options": {"$top": PAGE_SIZE, "resultFormat": "objectArray"}});
        if let Some(token) = token {
            body["options"]["$skipToken"] = token.into();
        }
        let output = az_output(&[
            "rest",
            "--method",
            "post",
            "--url",
            RESOURCE_GRAPH,
            "--body",
            &body.to_string(),
            "--output",
            "json",
        ])
        .await?;
        if !output.status.success() {
            return Err(Error::new(ErrorCode::QueryFailed, "Azure Resource Graph could not read this inventory page")
                .with_hint("Check `az login` and Resource Graph permissions, then retry"));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|_| Error::new(ErrorCode::QueryFailed, "Azure Resource Graph returned invalid JSON"))
    }

    async fn token(&self) -> Result<String> {
        let output = az_output(&[
            "account",
            "get-access-token",
            "--resource",
            "https://management.azure.com/",
            "--query",
            "accessToken",
            "--output",
            "tsv",
        ])
        .await?;
        if !output.status.success() || output.stdout.is_empty() {
            return Err(Error::new(ErrorCode::AuthFailed, "The Azure CLI could not issue a Resource Manager token")
                .with_hint("Run `az login`, then search again"));
        }
        let token = String::from_utf8(output.stdout)
            .map_err(|_| Error::new(ErrorCode::AuthFailed, "The Azure CLI returned an unreadable token"))?;
        if token.trim().is_empty() {
            return Err(Error::new(ErrorCode::AuthFailed, "The Azure CLI returned an empty token"));
        }
        Ok(token.trim().into())
    }

    async fn arm(&self, url: &str, token: &str) -> Result<Value> {
        let reply = crate::http::get_bearer_limited(
            url.into(),
            token.into(),
            "Listing Azure databases",
            crate::cloud_process::MAX_OUTPUT as u64,
        )
        .await?;
        if reply.status != 200 {
            return Err(Error::new(ErrorCode::QueryFailed, "Azure could not list this server's databases")
                .with_hint("Check the server state and Resource Manager permissions"));
        }
        Ok(reply.json)
    }
}

struct Search<'a> {
    terms: &'a [String],
    result: DiscoveryResult,
    seen: BTreeSet<(String, Option<u16>, String)>,
    requests: usize,
    deadline: Instant,
    success: bool,
    first_error: Option<Error>,
    full: bool,
}

impl<'a> Search<'a> {
    fn new(terms: &'a [String], deadline: Instant) -> Self {
        Self {
            terms,
            result: DiscoveryResult::default(),
            seen: BTreeSet::new(),
            requests: 0,
            deadline,
            success: false,
            first_error: None,
            full: false,
        }
    }

    fn exhausted(&self) -> bool {
        self.requests >= MAX_REQUESTS || Instant::now() >= self.deadline
    }

    async fn request<T>(&mut self, future: impl Future<Output = Result<T>>) -> Result<T> {
        if self.exhausted() {
            return Err(Error::new(
                ErrorCode::Timeout,
                "Discovery reached its search budget; narrow the search or choose specific regions",
            ));
        }
        self.requests += 1;
        tokio::time::timeout_at(self.deadline.min(Instant::now() + REQUEST_TIMEOUT), future).await.map_err(|_| {
            Error::new(ErrorCode::Timeout, "Cloud discovery timed out; narrow the search or choose specific regions")
        })?
    }

    fn accept(&mut self, rows: impl IntoIterator<Item = DiscoveredDatabase>) -> bool {
        for row in rows {
            if !self.terms.iter().all(|term| matches_term(&row.database, term) || matches_term(&row.server, term)) {
                continue;
            }
            let key = (row.host.to_ascii_lowercase(), row.port, row.database.clone());
            if !self.seen.insert(key) {
                continue;
            }
            if self.result.items.len() == DISCOVERY_LIMIT {
                self.result.has_more = true;
                self.full = true;
                return true;
            }
            self.result.items.push(row);
        }
        false
    }

    fn warning(&mut self, error: Error) -> Result<()> {
        if error.code == ErrorCode::AuthFailed {
            return Err(error);
        }
        self.result.has_more = true;
        let message: String = error.message.chars().take(300).collect();
        if self.result.warnings.len() < 3 && !self.result.warnings.contains(&message) {
            self.result.warnings.push(message);
        }
        self.first_error.get_or_insert(error);
        Ok(())
    }

    fn finish(mut self) -> Result<DiscoveryResult> {
        if self.exhausted() && !self.full {
            self.warning(Error::new(
                ErrorCode::Timeout,
                "Discovery reached its search budget; more matches may exist",
            ))?;
        }
        if !self.success
            && let Some(error) = self.first_error
        {
            return Err(error);
        }
        self.result
            .items
            .sort_by(|a, b| a.database.cmp(&b.database).then(a.server.cmp(&b.server)).then(a.host.cmp(&b.host)));
        assign_aliases(&mut self.result.items);
        Ok(self.result)
    }
}

#[derive(Default)]
struct Cursor {
    current: Option<String>,
    seen: BTreeSet<String>,
}
impl Cursor {
    fn advance(&mut self, value: Option<String>) -> Result<bool> {
        let Some(value) = value else {
            self.current = None;
            return Ok(false);
        };
        if value.len() > MAX_CURSOR || value.chars().any(char::is_control) || !self.seen.insert(value.clone()) {
            return Err(Error::new(
                ErrorCode::QueryFailed,
                "The cloud returned an invalid or repeated continuation token",
            ));
        }
        self.current = Some(value);
        Ok(true)
    }
}

fn cursor(value: &Value, field: &str) -> Result<Option<String>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(Error::new(ErrorCode::QueryFailed, "The cloud returned an unreadable continuation token")),
    }
}

fn entries<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| Error::new(ErrorCode::QueryFailed, "The cloud returned an unreadable inventory page"))
}

fn graph_entries(value: &Value) -> Result<&[Value]> {
    let rows = entries(value, "data")?;
    if rows.len() > PAGE_SIZE {
        return Err(Error::new(ErrorCode::QueryFailed, "Azure exceeded the requested inventory page size"));
    }
    Ok(rows)
}

fn graph_cursor(value: &Value) -> Result<Option<String>> {
    let token = cursor(value, "$skipToken")?;
    let truncated = value.get("resultTruncated").is_some_and(|v| v == true || v == "true");
    if truncated && token.is_none() {
        return Err(Error::new(
            ErrorCode::QueryFailed,
            "Azure truncated the inventory without a continuation token; refine the search",
        ));
    }
    Ok(token)
}

pub(super) async fn resource_graph<T: serde::de::DeserializeOwned>(query: &str) -> Result<Vec<T>> {
    let value = Live.graph(query, None).await?;
    graph_entries(&value)?
        .iter()
        .cloned()
        .map(|row| {
            serde_json::from_value(row)
                .map_err(|_| Error::new(ErrorCode::QueryFailed, "Azure returned an unreadable resource"))
        })
        .collect()
}

pub(super) async fn azure(terms: &[String]) -> Result<DiscoveryResult> {
    validate_terms(terms)?;
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let _permit = tokio::time::timeout_at(deadline, SEARCHES.acquire())
        .await
        .map_err(|_| Error::new(ErrorCode::Timeout, "Cloud discovery is busy; retry shortly"))?
        .map_err(|_| Error::internal("Cloud discovery is unavailable"))?;
    azure_with(&Live, terms, deadline).await
}

async fn azure_with(transport: &impl Transport, terms: &[String], deadline: Instant) -> Result<DiscoveryResult> {
    let mut search = Search::new(terms, deadline);
    if let Err(error) = azure_sql(transport, &mut search).await {
        search.warning(error)?;
    }
    if !search.full
        && !search.exhausted()
        && let Err(error) = azure_flexible(transport, &mut search).await
    {
        search.warning(error)?;
    }
    search.finish()
}

async fn azure_sql(transport: &impl Transport, search: &mut Search<'_>) -> Result<()> {
    let query = query(search.terms);
    let mut page = Cursor::default();
    loop {
        let value = search.request(transport.graph(&query, page.current.as_deref())).await?;
        let data = graph_entries(&value)?;
        search.success = true;
        for row in data {
            let row: Row = serde_json::from_value(row.clone())
                .map_err(|_| Error::new(ErrorCode::QueryFailed, "Azure returned an unreadable SQL database"))?;
            let host = row
                .fqdn
                .filter(|host| !host.is_empty())
                .unwrap_or_else(|| format!("{}.database.windows.net", row.server));
            if search.accept([DiscoveredDatabase {
                alias: String::new(),
                cloud: Cloud::Azure,
                engine: Engine::Mssql,
                environment: infer_environment(&format!("{} {}", row.database, row.server)),
                server: row.server,
                host,
                database: row.database,
                location: row.location,
                subscription: row.subscription,
                resource_group: row.resource_group,
                port: Some(1433),
                user: None,
                region: None,
                auth: AuthKind::Entra,
                saved_as: None,
            }]) {
                return Ok(());
            }
        }
        if !page.advance(graph_cursor(&value)?)? {
            return Ok(());
        }
    }
}

/// Continuations may only read the same ARM collection on the same host. Bearer tokens never follow arbitrary URLs.
fn arm_next(initial: &str, next: String) -> Result<String> {
    let next = if next.starts_with("/subscriptions/") { format!("{MANAGEMENT}{next}") } else { next };
    if next.len() > MAX_CURSOR
        || next.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\' || c == '#')
        || !next.starts_with("https://management.azure.com/")
        || !next
            .split('?')
            .next()
            .unwrap_or_default()
            .eq_ignore_ascii_case(initial.split('?').next().unwrap_or_default())
    {
        return Err(Error::new(
            ErrorCode::PermissionDenied,
            "Azure returned a continuation outside the requested resource collection",
        ));
    }
    Ok(next)
}

async fn mysql_entra(transport: &impl Transport, search: &mut Search<'_>, url: &str, bearer: &str) -> Result<bool> {
    let mut next = url.to_string();
    let mut page = Cursor::default();
    loop {
        let value = search.request(transport.arm(&next, bearer)).await?;
        if !entries(&value, "value")?.is_empty() {
            return Ok(true);
        }
        let token = cursor(&value, "nextLink")?.map(|link| arm_next(url, link)).transpose()?;
        if !page.advance(token)? {
            return Ok(false);
        }
        next.clone_from(page.current.as_ref().expect("advanced"));
    }
}

async fn flexible_children(
    transport: &impl Transport,
    search: &mut Search<'_>,
    server: &FlexibleServer,
    engine: Engine,
    bearer: &str,
) -> Result<()> {
    let url = flexible_server_url(&server.id, "databases", engine)
        .ok_or_else(|| Error::new(ErrorCode::QueryFailed, "Azure returned an invalid flexible-server resource id"))?;
    let entra = if engine == Engine::Postgres {
        server.entra.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("enabled"))
    } else if let Some(admins) = flexible_server_url(&server.id, "administrators", engine) {
        match mysql_entra(transport, search, &admins, bearer).await {
            Ok(enabled) => enabled,
            Err(error) => {
                search.warning(error)?;
                false
            }
        }
    } else {
        false
    };
    let mut next = url.clone();
    let mut page = Cursor::default();
    let mut application_database = false;
    loop {
        let value = search.request(transport.arm(&next, bearer)).await?;
        let names = entries(&value, "value")?;
        for name in names {
            let database = name
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::new(ErrorCode::QueryFailed, "Azure returned an unreadable database name"))?;
            if system_database(engine, database) || (engine == Engine::Postgres && database == "postgres") {
                continue;
            }
            application_database = true;
            if search.accept([flexible_database(server, engine, database.into(), entra)]) {
                return Ok(());
            }
        }
        let token = cursor(&value, "nextLink")?.map(|link| arm_next(&url, link)).transpose()?;
        if !page.advance(token)? {
            break;
        }
        next.clone_from(page.current.as_ref().expect("advanced"));
    }
    if !application_database {
        search.accept([flexible_database(server, engine, default_database(engine).into(), entra)]);
    }
    Ok(())
}

async fn azure_flexible(transport: &impl Transport, search: &mut Search<'_>) -> Result<()> {
    let mut page = Cursor::default();
    let mut bearer = None;
    loop {
        let value = search.request(transport.graph(FLEXIBLE_QUERY, page.current.as_deref())).await?;
        let servers = graph_entries(&value)?;
        search.success = true;
        for server in servers {
            let server: FlexibleServer = serde_json::from_value(server.clone())
                .map_err(|_| Error::new(ErrorCode::QueryFailed, "Azure returned an unreadable flexible server"))?;
            let Some(engine) = flexible_engine(&server.kind) else { continue };
            if server.state.as_deref().is_some_and(|v| !v.eq_ignore_ascii_case("ready")) {
                search.warning(Error::new(
                    ErrorCode::QueryFailed,
                    "Some Azure servers are not ready; their databases could not be listed",
                ))?;
                let entra = engine == Engine::Postgres
                    && server.entra.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("enabled"));
                search.accept([flexible_database(&server, engine, default_database(engine).into(), entra)]);
            } else {
                if bearer.is_none() {
                    bearer = Some(search.request(transport.token()).await?);
                }
                let before = search.result.items.len();
                if let Err(error) =
                    flexible_children(transport, search, &server, engine, bearer.as_deref().expect("token acquired"))
                        .await
                {
                    search.warning(error)?;
                    if search.result.items.len() == before {
                        let entra = engine == Engine::Postgres
                            && server.entra.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("enabled"));
                        search.accept([flexible_database(&server, engine, default_database(engine).into(), entra)]);
                    }
                }
            }
            if search.full || search.exhausted() {
                return Ok(());
            }
        }
        if !page.advance(graph_cursor(&value)?)? {
            return Ok(());
        }
    }
}

pub(super) async fn aws_rds(regions: &[String], terms: &[String]) -> Result<DiscoveryResult> {
    validate_terms(terms)?;
    if regions.len() > MAX_REGIONS || regions.iter().any(|region| !aws::valid_region(region)) {
        return Err(Error::invalid("Use at most 100 AWS region codes, such as us-east-1 or eu-central-1"));
    }
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let _permit = tokio::time::timeout_at(deadline, SEARCHES.acquire())
        .await
        .map_err(|_| Error::new(ErrorCode::Timeout, "Cloud discovery is busy; retry shortly"))?
        .map_err(|_| Error::internal("Cloud discovery is unavailable"))?;
    aws_with(&Live, regions, terms, deadline).await
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).into()).collect()
}

async fn enabled_regions(transport: &impl Transport, search: &mut Search<'_>) -> Result<Vec<String>> {
    let list = search
        .request(transport.aws(&args(&[
            "account",
            "list-regions",
            "--region-opt-status-contains",
            "ENABLED",
            "ENABLED_BY_DEFAULT",
            "--region",
            "us-east-1",
            "--max-items",
            "100",
            "--page-size",
            "50",
            "--output",
            "json",
        ])))
        .await;
    let value = match list {
        Ok(value) => value,
        Err(error) if error.code == ErrorCode::AuthFailed => return Err(error),
        Err(_) => {
            search
                .request(transport.aws(&args(&[
                    "ec2",
                    "describe-regions",
                    "--region",
                    "us-east-1",
                    "--no-paginate",
                    "--output",
                    "json",
                ])))
                .await?
        }
    };
    let mut regions: Vec<_> = entries(&value, "Regions")?
        .iter()
        .filter_map(|row| row.get("RegionName").and_then(Value::as_str))
        .filter(|name| aws::valid_region(name))
        .take(MAX_REGIONS)
        .map(str::to_string)
        .collect();
    if cursor(&value, "NextToken")?.is_some() || entries(&value, "Regions")?.len() > MAX_REGIONS {
        search
            .warning(Error::new(ErrorCode::QueryFailed, "The region listing is incomplete; choose specific regions"))?;
    }
    if regions.is_empty() {
        return Err(Error::new(ErrorCode::QueryFailed, "AWS returned no enabled regions"));
    }
    regions.sort();
    regions.dedup();
    Ok(regions)
}

fn rds_args(operation: &str, region: &str, token: Option<&str>) -> Vec<String> {
    let size = PAGE_SIZE.to_string();
    let mut out =
        args(&["rds", operation, "--region", region, "--page-size", &size, "--max-items", &size, "--output", "json"]);
    if let Some(token) = token {
        out.push(format!("--starting-token={token}"));
    }
    out
}

async fn aws_with(
    transport: &impl Transport,
    regions: &[String],
    terms: &[String],
    deadline: Instant,
) -> Result<DiscoveryResult> {
    let mut search = Search::new(terms, deadline);
    let identity = search.request(transport.aws(&args(&["sts", "get-caller-identity", "--output", "json"]))).await?;
    let account = identity
        .get("Account")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::new(ErrorCode::QueryFailed, "AWS returned no account identity"))?;
    let mut regions =
        if regions.is_empty() { enabled_regions(transport, &mut search).await? } else { regions.to_vec() };
    regions.sort();
    regions.dedup();
    for region in regions {
        for (operation, field) in [("describe-db-instances", "DBInstances"), ("describe-db-clusters", "DBClusters")] {
            let mut page = Cursor::default();
            loop {
                let response =
                    search.request(transport.aws(&rds_args(operation, &region, page.current.as_deref()))).await;
                let step = response.and_then(|value| {
                    if entries(&value, field)?.len() > PAGE_SIZE {
                        return Err(Error::new(
                            ErrorCode::QueryFailed,
                            "AWS exceeded the requested inventory page size",
                        ));
                    }
                    search.success = true;
                    let rows = if field == "DBInstances" {
                        rds_rows(&region, account, &value, &Value::Null)
                    } else {
                        rds_rows(&region, account, &Value::Null, &value)
                    };
                    if search.accept(rows) {
                        return Ok(false);
                    }
                    page.advance(cursor(&value, "NextToken")?)
                });
                match step {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        search.warning(error)?;
                        break;
                    }
                }
            }
            if search.full || search.exhausted() {
                return search.finish();
            }
        }
    }
    search.finish()
}

#[cfg(test)]
mod tests;
