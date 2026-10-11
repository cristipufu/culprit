//! The few HTTPS calls Culprit makes itself: Microsoft Entra ID sign-in and Azure Resource Manager listings. HTTPS only, no
//! redirects (a redirect would carry a bearer token or a sign-in code elsewhere), short timeouts. Blocking calls run on the
//! blocking pool.

use std::sync::OnceLock;
use std::time::Duration;

use culprit_core::{Error, ErrorCode, Result};
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("culprit/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

/// A JSON response with its status. Non-JSON bodies become `Value::Null`.
pub struct Reply {
    pub status: u16,
    pub json: Value,
}

fn finish(result: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>, what: &str) -> Result<Reply> {
    let mut response = result.map_err(|e| {
        Error::new(ErrorCode::ConnectionFailed, format!("{what} failed: {e}"))
            .with_hint("Check the network connection and any proxy, then retry")
    })?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    Ok(Reply { status, json: serde_json::from_str(&text).unwrap_or(Value::Null) })
}

/// POST an `application/x-www-form-urlencoded` body.
pub async fn post_form(url: String, fields: Vec<(&'static str, String)>, what: &'static str) -> Result<Reply> {
    tokio::task::spawn_blocking(move || {
        let form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        finish(agent().post(&url).header("Accept", "application/json").send_form(form), what)
    })
    .await
    .map_err(|e| Error::internal(e.to_string()))?
}

/// GET with a bearer token.
pub async fn get_bearer(url: String, token: String, what: &'static str) -> Result<Reply> {
    tokio::task::spawn_blocking(move || {
        finish(
            agent()
                .get(&url)
                .header("Authorization", &format!("Bearer {token}"))
                .header("Accept", "application/json")
                .call(),
            what,
        )
    })
    .await
    .map_err(|e| Error::internal(e.to_string()))?
}
