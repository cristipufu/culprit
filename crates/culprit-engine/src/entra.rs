//! Microsoft Entra ID tokens, so no database secret is ever stored. The Azure CLI's signed-in account first; when the CLI
//! is missing or signed out, a device-code sign-in (OAuth 2.0 device authorization grant) as a public client. Access tokens
//! stay in memory; the refresh token goes to the secret store (the OS keychain) and is replaced every time it is used.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use culprit_core::{ConnectionSpec, Engine, Error, ErrorCode, Result, entra_host_allowed};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use ts_rs::TS;

use crate::http::{self, Reply};

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
use crate::secrets::SecretStore;

const LOGIN_HOST: &str = "https://login.microsoftonline.com";
/// The Azure CLI's public client id: the default Azure.Identity's `DeviceCodeCredential` uses too.
pub const AZURE_CLI_CLIENT_ID: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";
const DEFAULT_TENANT: &str = "organizations";
/// Refresh an in-memory access token this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Never poll faster than the server asks, nor slower than this.
const MAX_INTERVAL_SECS: u64 = 30;

/// Token audience per engine: Azure SQL and Azure Database for PostgreSQL / MySQL use different resources.
fn resource(engine: Engine) -> &'static str {
    match engine {
        Engine::Mssql => "https://database.windows.net/",
        Engine::Postgres | Engine::Mysql => "https://ossrdbms-aad.database.windows.net",
        // A file has no server to sign in to.
        Engine::Yugabyte | Engine::Sqlite => "",
    }
}

fn scope(engine: Engine) -> String {
    format!("{}/.default offline_access", resource(engine).trim_end_matches('/'))
}

/// What the person needs to finish a device-code sign-in.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DeviceCodePrompt {
    pub user_code: String,
    pub verification_uri: String,
    /// Microsoft's own instructions, e.g. "To sign in, use a web browser to open the page … and enter the code …".
    pub message: String,
    #[ts(type = "number")]
    pub expires_in_secs: u64,
}

/// Device-code sign-in progress, for whoever shows it (the CLI prints, the app opens a dialog).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
#[ts(export)]
pub enum SignInEvent {
    Code { prompt: DeviceCodePrompt },
    Done,
    Failed { message: String },
}

pub type SignInHandler = Arc<dyn Fn(SignInEvent) + Send + Sync>;

/// How the sign-in endpoints are reached; a test double replaces it.
pub trait LoginEndpoint: Send + Sync {
    fn post_form(&self, url: String, fields: Vec<(&'static str, String)>) -> BoxFuture<'static, Result<Reply>>;
}

struct Https;

impl LoginEndpoint for Https {
    fn post_form(&self, url: String, fields: Vec<(&'static str, String)>) -> BoxFuture<'static, Result<Reply>> {
        Box::pin(http::post_form(url, fields, "Microsoft Entra ID sign-in"))
    }
}

/// Which account signs in: tenant and public client id. Both end up in URLs and form fields, so both are validated.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Authority {
    pub tenant: String,
    pub client_id: String,
}

impl Authority {
    /// Connection options `entra-tenant` / `entra-client-id`, then `CULPRIT_ENTRA_TENANT` / `CULPRIT_ENTRA_CLIENT_ID`, then
    /// the defaults (`organizations`, the Azure CLI's client id).
    pub fn for_spec(spec: &ConnectionSpec) -> Result<Authority> {
        let pick = |option: &str, env: &str, default: &str| {
            spec.option(option)
                .map(str::to_string)
                .or_else(|| std::env::var(env).ok())
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Authority::new(
            &pick("entra-tenant", "CULPRIT_ENTRA_TENANT", DEFAULT_TENANT),
            &pick("entra-client-id", "CULPRIT_ENTRA_CLIENT_ID", AZURE_CLI_CLIENT_ID),
        )
    }

    pub fn new(tenant: &str, client_id: &str) -> Result<Authority> {
        let tenant = tenant.trim();
        let client_id = client_id.trim();
        let tenant_ok = !tenant.is_empty()
            && tenant.len() <= 64
            && tenant.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if !tenant_ok {
            return Err(Error::invalid(format!("'{tenant}' is not a Microsoft Entra tenant"))
                .with_hint("Use a tenant id (GUID), a domain like contoso.onmicrosoft.com, or organizations"));
        }
        if !is_guid(client_id) {
            return Err(Error::invalid(format!("'{client_id}' is not an application (client) id")));
        }
        Ok(Authority { tenant: tenant.to_ascii_lowercase(), client_id: client_id.to_ascii_lowercase() })
    }

    fn url(&self, path: &str) -> String {
        format!("{LOGIN_HOST}/{}/oauth2/v2.0/{path}", self.tenant)
    }

    /// The secret-store account holding this authority's refresh token. Never a valid connection name.
    pub fn refresh_account(&self) -> String {
        format!("entra-refresh:{}:{}", self.tenant, self.client_id)
    }
}

fn is_guid(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// How a connection gets its token: `auto` (Azure CLI, then device code), or one of them only. Connection option
/// `entra-login`, else `CULPRIT_ENTRA_LOGIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Login {
    Auto,
    AzureCli,
    DeviceCode,
}

fn login_mode(spec: &ConnectionSpec) -> Result<Login> {
    let value = spec.option("entra-login").map(str::to_string).or_else(|| std::env::var("CULPRIT_ENTRA_LOGIN").ok());
    match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("auto") => Ok(Login::Auto),
        Some("azure-cli" | "az") => Ok(Login::AzureCli),
        Some("device-code") => Ok(Login::DeviceCode),
        Some(other) => {
            Err(Error::invalid(format!("Unknown entra-login `{other}`"))
                .with_hint("Use auto, azure-cli or device-code"))
        }
    }
}

struct Cached {
    token: String,
    expires: Instant,
}

/// One sign-in at a time per authority and resource. A scan, the dashboard and live polling all ask for a token at
/// once when a connection opens; without this each started its own device-code sign-in, and every new code replaced
/// the one the person was typing.
#[derive(Default)]
struct Flight {
    turn: tokio::sync::Mutex<()>,
    /// How the last sign-in failed, and when. For a minute after, every caller gets that error instead of a new code:
    /// the callers that waited on it, and polling that would otherwise reopen a sign-in the person just cancelled.
    failure: Mutex<Option<(Instant, Error)>>,
}

/// How long a failed or cancelled sign-in answers for the next one. The Azure CLI is still tried first, so `az login`
/// works straight away.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// Token broker held by the workspace: the in-memory access-token cache, who shows device codes, and cancellation.
pub struct EntraAuth {
    endpoint: Arc<dyn LoginEndpoint>,
    handler: Mutex<Option<SignInHandler>>,
    cache: Mutex<HashMap<(Authority, &'static str), Cached>>,
    flights: Mutex<HashMap<(Authority, &'static str), Arc<Flight>>>,
    generation: AtomicU64,
}

impl Default for EntraAuth {
    fn default() -> Self {
        EntraAuth::with_endpoint(Arc::new(Https))
    }
}

impl EntraAuth {
    pub fn with_endpoint(endpoint: Arc<dyn LoginEndpoint>) -> EntraAuth {
        EntraAuth {
            endpoint,
            handler: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Who shows device codes. Without one, a missing or signed-out Azure CLI is an error (scripts and agents never block
    /// waiting for a person).
    pub fn set_handler(&self, handler: Option<SignInHandler>) {
        if let Ok(mut h) = self.handler.lock() {
            *h = handler;
        }
    }

    /// Stop a device-code sign-in that is waiting for the person.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn emit(&self, event: SignInEvent) {
        let handler = self.handler.lock().ok().and_then(|h| h.clone());
        if let Some(handler) = handler {
            handler(event);
        }
    }

    fn has_handler(&self) -> bool {
        self.handler.lock().is_ok_and(|h| h.is_some())
    }

    /// Forget in-memory tokens and the stored refresh tokens for these authorities.
    pub fn sign_out(&self, authorities: &[Authority], secrets: &dyn SecretStore) -> Result<usize> {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
        if let Ok(mut flights) = self.flights.lock() {
            flights.clear();
        }
        let mut removed = 0;
        for authority in authorities {
            if secrets.get(&authority.refresh_account())?.is_some() {
                secrets.delete(&authority.refresh_account())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// A token for `spec`'s host, refused unless the host is an Azure database host for that engine.
    pub async fn token(&self, engine: Engine, spec: &ConnectionSpec, secrets: &dyn SecretStore) -> Result<String> {
        let host = spec.host.as_str();
        if !entra_host_allowed(engine, host) {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                format!("Refusing to send a Microsoft Entra ID token to '{host}', which is not an Azure {engine} host"),
            )
            .with_hint("Entra ID authentication only works with Azure database hosts; use password authentication for other servers"));
        }
        let mode = login_mode(spec)?;
        let cli_error = if mode == Login::DeviceCode {
            None
        } else {
            match azure_cli_token(engine).await {
                Ok(token) => return Ok(token),
                Err(e) if mode == Login::AzureCli => return Err(e),
                Err(e) => Some(e),
            }
        };
        let authority = Authority::for_spec(spec)?;
        match self.device_token(engine, &authority, secrets).await {
            Ok(token) => Ok(token),
            Err(e) if e.code == ErrorCode::Unsupported => Err(cli_error.unwrap_or(e)),
            Err(e) => Err(e),
        }
    }

    /// A cached access token, a refreshed one, or (with a handler) a fresh device-code sign-in. Callers that arrive
    /// while a sign-in for the same authority and resource is under way wait for it and use its token, or share its
    /// error, so the person only ever sees one code; and a sign-in that just failed or was cancelled isn't reopened
    /// for a minute.
    pub async fn device_token(
        &self,
        engine: Engine,
        authority: &Authority,
        secrets: &dyn SecretStore,
    ) -> Result<String> {
        let key = (authority.clone(), resource(engine));
        if let Some(token) = self.cached(&key) {
            return Ok(token);
        }
        let flight = self.flight(&key);
        let _turn = flight.turn.lock().await;
        if let Some(token) = self.cached(&key) {
            return Ok(token);
        }
        if let Some((at, failure)) = flight.failure.lock().ok().and_then(|f| f.clone())
            && at.elapsed() < ASK_AGAIN_AFTER
        {
            return Err(failure);
        }
        let result = self.refresh_or_sign_in(engine, authority, key, secrets).await;
        if let Ok(mut failure) = flight.failure.lock() {
            *failure = result.as_ref().err().map(|e| (Instant::now(), e.clone()));
        }
        result
    }

    fn flight(&self, key: &(Authority, &'static str)) -> Arc<Flight> {
        let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
        flights.entry(key.clone()).or_default().clone()
    }

    async fn refresh_or_sign_in(
        &self,
        engine: Engine,
        authority: &Authority,
        key: (Authority, &'static str),
        secrets: &dyn SecretStore,
    ) -> Result<String> {
        let account = authority.refresh_account();
        if let Some(refresh) = secrets.get(&account)? {
            let fields = vec![
                ("client_id", authority.client_id.clone()),
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", refresh),
                ("scope", scope(engine)),
            ];
            let reply = self.endpoint.post_form(authority.url("token"), fields).await?;
            match parse_token(&reply) {
                Ok(tokens) => return self.keep(key, tokens, &account, secrets),
                // Revoked, expired or used elsewhere: forget it and sign in again.
                Err(Poll::Failed(_)) => secrets.delete(&account)?,
                Err(Poll::Pending | Poll::SlowDown) => {}
            }
        }
        if !self.has_handler() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "Microsoft Entra ID sign-in needs a person at the keyboard",
            )
            .with_hint("Run `az login`, or run the command in a terminal to sign in with a device code"));
        }
        self.sign_in(engine, authority, key, &account, secrets).await
    }

    async fn sign_in(
        &self,
        engine: Engine,
        authority: &Authority,
        key: (Authority, &'static str),
        account: &str,
        secrets: &dyn SecretStore,
    ) -> Result<String> {
        let generation = self.generation.load(Ordering::SeqCst);
        let fields = vec![("client_id", authority.client_id.clone()), ("scope", scope(engine))];
        let reply = self.endpoint.post_form(authority.url("devicecode"), fields).await?;
        let (prompt, device_code, mut interval) = parse_device_code(&reply)?;
        let deadline = Instant::now() + Duration::from_secs(prompt.expires_in_secs);
        self.emit(SignInEvent::Code { prompt });
        let fail = |message: String| {
            self.emit(SignInEvent::Failed { message: message.clone() });
            Err(Error::new(ErrorCode::AuthFailed, message))
        };
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if self.generation.load(Ordering::SeqCst) != generation {
                return fail("Sign-in was cancelled".into());
            }
            if Instant::now() >= deadline {
                return fail("The sign-in code expired before it was used".into());
            }
            let fields = vec![
                ("client_id", authority.client_id.clone()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code".to_string()),
                ("device_code", device_code.clone()),
            ];
            let reply = self.endpoint.post_form(authority.url("token"), fields).await?;
            match parse_token(&reply) {
                Ok(tokens) => {
                    let token = self.keep(key, tokens, account, secrets)?;
                    self.emit(SignInEvent::Done);
                    return Ok(token);
                }
                Err(Poll::Pending) => {}
                Err(Poll::SlowDown) => interval = (interval + 5).min(MAX_INTERVAL_SECS),
                Err(Poll::Failed(message)) => return fail(message),
            }
        }
    }

    fn cached(&self, key: &(Authority, &'static str)) -> Option<String> {
        let cache = self.cache.lock().ok()?;
        cache.get(key).filter(|c| c.expires > Instant::now() + EXPIRY_MARGIN).map(|c| c.token.clone())
    }

    /// Cache the access token and replace the stored refresh token with the new one.
    fn keep(
        &self,
        key: (Authority, &'static str),
        tokens: Tokens,
        account: &str,
        secrets: &dyn SecretStore,
    ) -> Result<String> {
        if let Some(refresh) = tokens.refresh.as_deref() {
            secrets.set(account, refresh)?;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, Cached { token: tokens.access.clone(), expires: Instant::now() + tokens.expires_in });
        }
        Ok(tokens.access)
    }
}

struct Tokens {
    access: String,
    refresh: Option<String>,
    expires_in: Duration,
}

enum Poll {
    Pending,
    SlowDown,
    Failed(String),
}

fn seconds(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// `error_description` without trace and correlation ids; never echoes tokens.
fn description(json: &Value) -> String {
    let text = json.get("error_description").and_then(Value::as_str).unwrap_or("");
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        json.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string()
    } else {
        first.to_string()
    }
}

fn parse_token(reply: &Reply) -> std::result::Result<Tokens, Poll> {
    let json = &reply.json;
    if reply.status == 200
        && let Some(access) = json.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty())
    {
        return Ok(Tokens {
            access: access.to_string(),
            refresh: json.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string),
            expires_in: Duration::from_secs(json.get("expires_in").and_then(seconds).unwrap_or(3600)),
        });
    }
    match json.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Err(Poll::Pending),
        Some("slow_down") => Err(Poll::SlowDown),
        Some("authorization_declined") => Err(Poll::Failed("Sign-in was declined".into())),
        Some("expired_token") => Err(Poll::Failed("The sign-in code expired before it was used".into())),
        _ => Err(Poll::Failed(format!("Microsoft Entra ID refused the sign-in: {}", description(json)))),
    }
}

fn parse_device_code(reply: &Reply) -> Result<(DeviceCodePrompt, String, u64)> {
    let json = &reply.json;
    let field = |k: &str| json.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let (Some(device_code), Some(user_code), Some(uri)) =
        (field("device_code"), field("user_code"), field("verification_uri"))
    else {
        return Err(Error::new(
            ErrorCode::AuthFailed,
            format!("Microsoft Entra ID did not start a sign-in: {}", description(json)),
        )
        .with_hint("Check the tenant (connection option entra-tenant) and try again"));
    };
    if !uri.starts_with("https://") {
        return Err(Error::new(ErrorCode::AuthFailed, "Microsoft Entra ID returned an unexpected sign-in page"));
    }
    let expires = json.get("expires_in").and_then(seconds).unwrap_or(900);
    let interval = json.get("interval").and_then(seconds).unwrap_or(5).min(MAX_INTERVAL_SECS);
    let message = field("message").unwrap_or_else(|| format!("To sign in, open {uri} and enter the code {user_code}."));
    Ok((
        DeviceCodePrompt { user_code, verification_uri: uri, message, expires_in_secs: expires },
        device_code,
        interval,
    ))
}

/// The signed-in person's name inside a token (`upn`, `preferred_username`, `unique_name`), for PostgreSQL and MySQL,
/// which need it as the user name. The database verifies the token itself; this only reads the claim.
pub fn token_user(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    ["upn", "preferred_username", "unique_name"]
        .iter()
        .find_map(|k| claims.get(*k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string))
}

/// A token from the Azure CLI's signed-in account (the CLI caches and refreshes it).
pub async fn azure_cli_token(engine: Engine) -> Result<String> {
    let output = Command::new(az_binary())
        .args(["account", "get-access-token", "--resource", resource(engine), "--query", "accessToken"])
        .args(["--output", "tsv"])
        .output()
        .await
        .map_err(|e| {
            Error::new(ErrorCode::AuthFailed, format!("Could not run the Azure CLI: {e}"))
                .with_hint("Install the Azure CLI and run `az login`, or use password authentication")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(ErrorCode::AuthFailed, format!("Azure CLI could not issue a token: {}", stderr.trim()))
            .with_hint("Run `az login` (and `az account set --subscription …` if needed), then retry"));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(Error::new(ErrorCode::AuthFailed, "Azure CLI returned an empty token")
            .with_hint("Run `az login` and retry"));
    }
    Ok(token)
}

fn az_binary() -> &'static str {
    if cfg!(windows) { "az.cmd" } else { "az" }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::secrets::Memory;

    type Request = (String, Vec<(&'static str, String)>);

    /// Plays back sign-in endpoint replies and records the requests.
    struct Script {
        replies: Mutex<VecDeque<(u16, Value)>>,
        requests: Mutex<Vec<Request>>,
    }

    impl Script {
        fn new(replies: Vec<(u16, Value)>) -> Arc<Script> {
            Arc::new(Script { replies: Mutex::new(replies.into()), requests: Mutex::new(Vec::new()) })
        }
        fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl LoginEndpoint for Script {
        fn post_form(&self, url: String, fields: Vec<(&'static str, String)>) -> BoxFuture<'static, Result<Reply>> {
            self.requests.lock().unwrap().push((url, fields));
            let (status, json) = self.replies.lock().unwrap().pop_front().expect("an unexpected request");
            Box::pin(async move { Ok(Reply { status, json }) })
        }
    }

    fn field<'a>(fields: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
        fields.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str())
    }

    fn authority() -> Authority {
        Authority::new("organizations", AZURE_CLI_CLIENT_ID).unwrap()
    }

    fn device_code() -> (u16, Value) {
        (
            200,
            serde_json::json!({ "device_code": "dc-1", "user_code": "ABCD-EFGH", "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 900, "interval": 0, "message": "To sign in, use a web browser to open the page https://microsoft.com/devicelogin and enter the code ABCD-EFGH to authenticate." }),
        )
    }

    fn tokens(access: &str, refresh: &str) -> (u16, Value) {
        (
            200,
            serde_json::json!({ "token_type": "Bearer", "access_token": access, "refresh_token": refresh, "expires_in": 3599 }),
        )
    }

    #[tokio::test]
    async fn device_code_sign_in_polls_then_keeps_the_refresh_token() {
        let script = Script::new(vec![
            device_code(),
            (400, serde_json::json!({ "error": "authorization_pending" })),
            (400, serde_json::json!({ "error": "authorization_pending" })),
            tokens("access-1", "refresh-1"),
        ]);
        let auth = EntraAuth::with_endpoint(script.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        auth.set_handler(Some(Arc::new(move |e| seen.lock().unwrap().push(e))));
        let secrets = Memory::default();

        let token = auth.device_token(Engine::Mssql, &authority(), &secrets).await.unwrap();
        assert_eq!(token, "access-1");
        assert_eq!(secrets.get(&authority().refresh_account()).unwrap().as_deref(), Some("refresh-1"));
        let events = events.lock().unwrap().clone();
        assert!(matches!(&events[0], SignInEvent::Code { prompt } if prompt.user_code == "ABCD-EFGH"));
        assert_eq!(events.last(), Some(&SignInEvent::Done));

        let requests = script.requests();
        assert_eq!(requests[0].0, "https://login.microsoftonline.com/organizations/oauth2/v2.0/devicecode");
        assert_eq!(field(&requests[0].1, "scope"), Some("https://database.windows.net/.default offline_access"));
        assert_eq!(field(&requests[1].1, "device_code"), Some("dc-1"));
        assert!(requests.iter().all(|(_, f)| field(f, "client_secret").is_none()), "a public client sends no secret");

        // The access token is reused from memory until it nears expiry: no further requests.
        assert_eq!(auth.device_token(Engine::Mssql, &authority(), &secrets).await.unwrap(), "access-1");
        assert_eq!(script.requests().len(), 4);
    }

    // Opening a connection asks for a token from several places at once (the report, the dashboard, live polling). They
    // share one sign-in: one code on screen, and every caller gets its token.
    #[tokio::test]
    async fn concurrent_callers_share_one_device_code_sign_in() {
        let script = Script::new(vec![
            device_code(),
            (400, serde_json::json!({ "error": "authorization_pending" })),
            (400, serde_json::json!({ "error": "authorization_pending" })),
            tokens("access-1", "refresh-1"),
        ]);
        let auth = EntraAuth::with_endpoint(script.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        auth.set_handler(Some(Arc::new(move |e| seen.lock().unwrap().push(e))));
        let secrets = Memory::default();

        let authority = authority();
        let (a, b, c) = tokio::join!(
            auth.device_token(Engine::Mssql, &authority, &secrets),
            auth.device_token(Engine::Mssql, &authority, &secrets),
            auth.device_token(Engine::Mssql, &authority, &secrets),
        );
        assert_eq!((a.unwrap(), b.unwrap(), c.unwrap()), ("access-1".into(), "access-1".into(), "access-1".into()));
        let codes = events.lock().unwrap().iter().filter(|e| matches!(e, SignInEvent::Code { .. })).count();
        assert_eq!(codes, 1, "one code on screen, not one per caller");
        assert_eq!(script.requests().iter().filter(|(url, _)| url.ends_with("/devicecode")).count(), 1);
    }

    // A sign-in that fails (declined, cancelled, expired) fails the callers waiting on it too, rather than each of them
    // putting a new code on screen straight away.
    #[tokio::test]
    async fn callers_waiting_on_a_failed_sign_in_share_its_error() {
        let script = Script::new(vec![
            device_code(),
            (400, serde_json::json!({ "error": "authorization_declined", "error_description": "The user declined." })),
        ]);
        let auth = EntraAuth::with_endpoint(script.clone());
        auth.set_handler(Some(Arc::new(|_| {})));
        let secrets = Memory::default();

        let authority = authority();
        let (a, b) = tokio::join!(
            auth.device_token(Engine::Mssql, &authority, &secrets),
            auth.device_token(Engine::Mssql, &authority, &secrets),
        );
        let (a, b) = (a.unwrap_err(), b.unwrap_err());
        assert_eq!((a.code, b.code), (ErrorCode::AuthFailed, ErrorCode::AuthFailed));
        assert_eq!(a.message, b.message);
        // And polling right after doesn't reopen it: the same answer, with no new code.
        let c = auth.device_token(Engine::Mssql, &authority, &secrets).await.unwrap_err();
        assert_eq!(c.message, a.message);
        assert_eq!(script.requests().len(), 2, "one code requested and one poll, nothing more");
    }

    #[tokio::test]
    async fn refresh_tokens_rotate_and_a_revoked_one_is_forgotten() {
        let secrets = Memory::default();
        secrets.set(&authority().refresh_account(), "refresh-old").unwrap();
        let script = Script::new(vec![tokens("access-2", "refresh-new")]);
        let auth = EntraAuth::with_endpoint(script.clone());
        assert_eq!(auth.device_token(Engine::Postgres, &authority(), &secrets).await.unwrap(), "access-2");
        assert_eq!(secrets.get(&authority().refresh_account()).unwrap().as_deref(), Some("refresh-new"));
        let request = &script.requests()[0];
        assert_eq!(field(&request.1, "refresh_token"), Some("refresh-old"));
        assert_eq!(
            field(&request.1, "scope"),
            Some("https://ossrdbms-aad.database.windows.net/.default offline_access")
        );

        // Revoked: forgotten, and with nobody to show a code to, the caller gets a clear error instead of a hang.
        let script = Script::new(vec![(
            400,
            serde_json::json!({ "error": "invalid_grant", "error_description": "AADSTS700082: The refresh token has expired.\r\nTrace ID: x" }),
        )]);
        let auth = EntraAuth::with_endpoint(script);
        let e = auth.device_token(Engine::Mysql, &authority(), &secrets).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported);
        assert_eq!(secrets.get(&authority().refresh_account()).unwrap(), None);
    }

    #[tokio::test]
    async fn declined_and_cancelled_sign_ins_fail_cleanly() {
        let script = Script::new(vec![device_code(), (400, serde_json::json!({ "error": "authorization_declined" }))]);
        let auth = EntraAuth::with_endpoint(script);
        auth.set_handler(Some(Arc::new(|_| {})));
        let e = auth.device_token(Engine::Mssql, &authority(), &Memory::default()).await.unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (ErrorCode::AuthFailed, "Sign-in was declined"));

        let script = Script::new(vec![device_code()]);
        let auth = Arc::new(EntraAuth::with_endpoint(script));
        let canceller = auth.clone();
        auth.set_handler(Some(Arc::new(move |e| {
            if matches!(e, SignInEvent::Code { .. }) {
                canceller.cancel();
            }
        })));
        let e = auth.device_token(Engine::Mssql, &authority(), &Memory::default()).await.unwrap_err();
        assert_eq!(e.message, "Sign-in was cancelled");
    }

    #[tokio::test]
    async fn tokens_never_go_to_other_hosts() {
        let auth = EntraAuth::with_endpoint(Script::new(vec![]));
        let spec = ConnectionSpec { host: "db.attacker.example".into(), ..Default::default() };
        let e = auth.token(Engine::Postgres, &spec, &Memory::default()).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn authorities_are_validated() {
        assert!(Authority::new("contoso.onmicrosoft.com", AZURE_CLI_CLIENT_ID).is_ok());
        assert!(Authority::new("00000000-0000-0000-0000-0000000000a1", AZURE_CLI_CLIENT_ID).is_ok());
        assert!(Authority::new("evil/../x?", AZURE_CLI_CLIENT_ID).is_err());
        assert!(Authority::new("organizations", "not-a-guid").is_err());
        assert_eq!(authority().refresh_account(), format!("entra-refresh:organizations:{AZURE_CLI_CLIENT_ID}"));
    }

    #[test]
    fn user_name_comes_from_the_token_claims() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"upn":"dev@example.com","oid":"x"}"#);
        assert_eq!(token_user(&format!("h.{claims}.s")).as_deref(), Some("dev@example.com"));
        assert_eq!(token_user("not-a-jwt"), None);
    }
}
