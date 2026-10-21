//! Database and server settings that commonly hurt performance or hide the data needed to diagnose it, per engine:
//! SQL Server (`mssql`), PostgreSQL (`postgres`), MySQL and MariaDB (`mysql`), and SQLite (`sqlite`).

use crate::evidence::Requirement as Evidence;
use culprit_core::waits::category_key;
use culprit_core::{
    Capability, Category, Check, CheckInfo, Confidence, Engine, Finding, Fix, Metric, ServerInfo, Severity, Snapshot,
    SubjectKind, Unit, format,
};

use crate::support::*;

mod mssql;
mod mysql;
mod postgres;
mod sqlite;
#[cfg(test)]
mod tests;

pub(crate) use mssql::{read_committed_snapshot_off, versioning_spares_read};
pub(crate) use mysql::MARIADB_TABLE_LOCK_TIME;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[&ConfigCheck];

pub fn config_info() -> CheckInfo {
    CheckInfo {
        id: "config".into(),
        title: "Configuration".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description:
            "Database and server settings that commonly hurt performance or hide the data needed to diagnose it.".into(),
    }
}

check!(ConfigCheck: config_info, config, &[Evidence::Settings]);

pub fn config(snapshot: &Snapshot) -> Vec<Finding> {
    match snapshot.server.engine {
        Engine::Mssql => mssql::check(snapshot),
        Engine::Postgres => postgres::check(snapshot),
        Engine::Mysql => mysql::check(snapshot),
        Engine::Sqlite => sqlite::check(snapshot),
    }
}

pub(crate) fn value<'a>(snapshot: &'a Snapshot, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| snapshot.setting(n).map(|s| s.value.as_str()))
}

pub(super) fn number(snapshot: &Snapshot, name: &str) -> Option<f64> {
    value(snapshot, &[name]).and_then(|v| v.trim().parse::<f64>().ok())
}

pub(crate) fn is_on(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

pub(crate) fn setting_finding(key: &str, severity: Severity, title: String, summary: String, fix: Fix) -> Finding {
    let mut f = finding("config", key, Category::Configuration, subject(SubjectKind::Setting, key));
    f.severity = severity;
    f.impact.score = base_score(severity) * 0.8;
    f.impact.label = "Setting".into();
    f.confidence = Confidence::Medium;
    f.title = title;
    f.summary = summary;
    f.fix = Some(fix);
    // A setting's finding reads the setting: once the change is in effect, it no longer fires. A setting read only at
    // startup says so ([`after_restart`]); a finding that reads a count or a share as well says its own way.
    f.verify = vec!["Re-scan once the change has taken effect: this finding is gone.".into()];
    f
}

/// The way to verify a setting the server reads only when it starts: the next scan after the restart (a reboot, on a
/// managed service) reads it.
pub(crate) fn after_restart(f: &mut Finding) {
    f.verify = vec!["Re-scan after the server has restarted with the change: this finding is gone.".into()];
}

/// Several settings changed together, worded the way [`setting_change`] words one: one sentence naming them all, and
/// the statements, on a server where they run.
pub(crate) fn settings_change(server: &ServerInfo, changes: &[(&str, &str)]) -> SettingChange {
    let Some(((last, last_value), lead)) = changes.split_last() else {
        return SettingChange { how: String::new(), sql: None };
    };
    let each: Vec<SettingChange> = changes.iter().map(|(n, v)| setting_change(server, n, v)).collect();
    let sql: Vec<String> = each.iter().filter_map(|c| c.sql.clone()).collect();
    // On a managed service a setting kept per database is set with SQL, the others in the platform's control: one
    // sentence can't say both, so each says its own.
    let per_database = server.platform().is_some_and(|p| p.settings_control().is_some())
        && changes.iter().any(|(n, v)| server.engine.spec().database_setting(server, n, v).is_some());
    if per_database {
        let how: Vec<String> = each.into_iter().map(|c| c.how).collect();
        return SettingChange { how: how.join(" "), sql: (!sql.is_empty()).then(|| sql.join("\n")) };
    }
    let name = match lead {
        [] => last.to_string(),
        _ => format!("{} and {last}", lead.iter().map(|(n, v)| format!("{n} to {v}")).collect::<Vec<_>>().join(", ")),
    };
    let mut change = setting_change(server, &name, last_value);
    change.sql = (!sql.is_empty()).then(|| sql.join("\n"));
    change
}

/// A SQL Server server option through [`setting_change`]: `sp_configure` with advanced options shown (it refuses
/// them otherwise) on a server you manage or a Managed Instance; the DB parameter group on RDS.
pub(crate) fn server_option(server: &ServerInfo, name: &str, value: &str) -> SettingChange {
    let mut change = setting_change(server, name, value);
    change.sql = change.sql.map(|sql| format!("EXEC sp_configure 'show advanced options', 1;\nRECONFIGURE;\n{sql}"));
    change
}
