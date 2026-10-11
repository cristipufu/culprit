//! Deadlocks pasted or dropped in (SQL Server .xdl XML, InnoDB status, Postgres log lines), and links from deadlock
//! fixes to the findings that are the same fix.

use culprit_core::{Deadlock, DeadlockReport, Error, Report, Result};

/// Largest pasted text Culprit reads.
const MAX_TEXT: usize = 8 * 1024 * 1024;

/// Every deadlock in `text`, whatever engine wrote it.
pub fn parse_deadlock(text: &str) -> Result<Vec<Deadlock>> {
    if text.len() > MAX_TEXT {
        return Err(Error::invalid("That text is larger than 8 MB; paste one deadlock at a time"));
    }
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    let found = if trimmed.starts_with('<') || trimmed.contains("<deadlock") {
        sql_server(trimmed)?
    } else if trimmed.contains("LATEST DETECTED DEADLOCK") || trimmed.contains("*** (1) TRANSACTION") {
        innodb(trimmed)
    } else if trimmed.contains("deadlock detected") {
        postgres(trimmed)
    } else {
        Vec::new()
    };
    if found.is_empty() {
        return Err(Error::invalid("No deadlock found in that text").with_hint(
            "Paste a SQL Server deadlock graph (.xdl XML), the LATEST DETECTED DEADLOCK section of SHOW ENGINE INNODB STATUS, \
             or the Postgres log lines from \"deadlock detected\" to STATEMENT.",
        ));
    }
    Ok(found
        .into_iter()
        .map(|mut d| {
            d.source = "pasted".into();
            d
        })
        .collect())
}

#[cfg(feature = "mssql")]
fn sql_server(text: &str) -> Result<Vec<Deadlock>> {
    culprit_mssql::deadlocks::parse_deadlock_xml(text, "pasted")
}

#[cfg(not(feature = "mssql"))]
fn sql_server(_: &str) -> Result<Vec<Deadlock>> {
    Ok(Vec::new())
}

#[cfg(feature = "mysql")]
fn innodb(text: &str) -> Vec<Deadlock> {
    // A pasted report doesn't say which time zone the server prints in, so its time is left out.
    if text.contains("LATEST DETECTED DEADLOCK") {
        culprit_mysql::deadlocks::parse_innodb_status(text, false)
    } else {
        culprit_mysql::deadlocks::parse_innodb_status(&format!("LATEST DETECTED DEADLOCK\n{text}"), false)
    }
}

#[cfg(not(feature = "mysql"))]
fn innodb(_: &str) -> Vec<Deadlock> {
    Vec::new()
}

#[cfg(feature = "postgres")]
fn postgres(text: &str) -> Vec<Deadlock> {
    culprit_postgres::deadlocks::parse_deadlock_log(text)
}

#[cfg(not(feature = "postgres"))]
fn postgres(_: &str) -> Vec<Deadlock> {
    Vec::new()
}

/// Keep a fix's finding link only when the latest report has that finding, so links never lead nowhere.
pub fn link_findings(report: &mut DeadlockReport, latest: Option<&Report>) {
    for d in &mut report.deadlocks {
        for fix in &mut d.fixes {
            if let Some(id) = fix.finding.as_deref()
                && !latest.is_some_and(|r| r.findings.iter().any(|f| f.id == id))
            {
                fix.finding = None;
            }
        }
    }
}
