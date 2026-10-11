//! Cancel or end another backend from Live. The pid travels as a bound parameter; Culprit's own backends are refused.

use culprit_core::{Error, Result, SessionAction, session_number};

use crate::connect::{Connector, INTERACTIVE, map_error};
use crate::rows::tagged;

pub(crate) async fn interrupt(connector: &Connector, session: &str, action: SessionAction) -> Result<()> {
    let pid =
        i32::try_from(session_number(session)?).map_err(|_| Error::invalid(format!("'{session}' is not a pid")))?;
    let client = connector.connect(INTERACTIVE).await?;
    let target = client
        .query_opt(
            &tagged(
                "SELECT backend_type, coalesce(application_name, '') AS application, pid = pg_backend_pid() AS own
                   FROM pg_stat_activity WHERE pid = $1",
            ),
            &[&pid],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| Error::not_found(format!("Session {pid} is gone")))?;
    let own: bool = target.get("own");
    let application: String = target.get("application");
    if own || application == "culprit" {
        return Err(Error::invalid(format!("Session {pid} is Culprit's own")));
    }
    let backend: Option<String> = target.get("backend_type");
    if backend.as_deref() != Some("client backend") {
        return Err(Error::invalid(format!("Session {pid} is a PostgreSQL {} process", backend.unwrap_or_default())));
    }
    let function = match action {
        SessionAction::Cancel => "pg_cancel_backend",
        SessionAction::Terminate => "pg_terminate_backend",
    };
    let row =
        client.query_one(&tagged(&format!("SELECT {function}($1) AS signalled")), &[&pid]).await.map_err(map_error)?;
    if !row.get::<_, bool>("signalled") {
        return Err(Error::not_found(format!("Session {pid} is gone")));
    }
    Ok(())
}
