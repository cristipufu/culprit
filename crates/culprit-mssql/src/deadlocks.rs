//! What blocked sessions wait for, and recent deadlocks: from the built-in system_health session, or on Azure SQL
//! Database from the deadlock telemetry it keeps per database. Both read DMVs and event targets only. The parser also
//! reads saved .xdl files; XML with a DTD is refused, so entities never resolve.

use culprit_core::deadlock::classify;
use culprit_core::{
    Deadlock, DeadlockProcess, DeadlockReport, DeadlockResource, Engine, Error, LockHolder, LockWait, Result,
};
use roxmltree::{Document, Node, ParsingOptions};

use crate::conn::{MsClient, query, query_one};

/// The lock each waiting task asked for. Objects in other databases are named only for whole-object locks; row and
/// page locks resolve through this database's partitions.
const LOCK_WAITS_SQL: &str = r#"
SELECT
  CAST(wt.session_id AS varchar(10)) AS session_id,
  l.request_mode AS mode,
  l.resource_type AS resource,
  CASE
    WHEN l.resource_type = 'OBJECT'
      THEN OBJECT_SCHEMA_NAME(l.resource_associated_entity_id, l.resource_database_id) + '.'
         + OBJECT_NAME(l.resource_associated_entity_id, l.resource_database_id)
    WHEN p.object_id IS NOT NULL THEN OBJECT_SCHEMA_NAME(p.object_id) + '.' + OBJECT_NAME(p.object_id)
  END AS object_name,
  i.name AS index_name
FROM sys.dm_os_waiting_tasks wt
JOIN sys.dm_tran_locks l ON l.lock_owner_address = wt.resource_address
LEFT JOIN sys.partitions p
  ON l.resource_type IN ('KEY', 'PAGE', 'RID', 'HOBT') AND l.resource_database_id = DB_ID()
 AND p.hobt_id = l.resource_associated_entity_id
LEFT JOIN sys.indexes i ON i.object_id = p.object_id AND i.index_id = p.index_id
WHERE wt.session_id IS NOT NULL AND wt.session_id <> @@SPID
"#;

pub(crate) async fn lock_waits(client: &mut MsClient) -> tiberius::Result<Vec<LockWait>> {
    let rows = query(client, LOCK_WAITS_SQL).await?;
    let mut out: Vec<LockWait> = Vec::new();
    for r in rows {
        let session = r.string("session_id");
        // A parallel request has one waiting task per thread; one lock per session is enough to explain it.
        if out.iter().any(|l| l.session == session) {
            continue;
        }
        out.push(LockWait {
            session,
            mode: r.str("mode"),
            resource: r.str("resource"),
            object: r.str("object_name"),
            index: r.str("index_name"),
        });
    }
    Ok(out)
}

const READ_LIMIT: usize = 50;

fn file_sql(hours: u32) -> String {
    format!(
        "SELECT TOP ({READ_LIMIT}) CONVERT(varchar(33), x.timestamp_utc, 127) AS at, CAST(x.event_data AS nvarchar(max)) AS event_xml
           FROM sys.fn_xe_file_target_read_file(N'system_health*.xel', NULL, NULL, NULL) x
          WHERE x.object_name = N'xml_deadlock_report' AND x.timestamp_utc >= DATEADD(hour, -{hours}, SYSUTCDATETIME())
          ORDER BY x.timestamp_utc DESC"
    )
}

const RING_SQL: &str = "SELECT CAST(t.target_data AS nvarchar(max)) AS target_xml
  FROM sys.dm_xe_session_targets t
  JOIN sys.dm_xe_sessions s ON s.address = t.event_session_address
 WHERE s.name = N'system_health' AND t.target_name = N'ring_buffer'";

/// Azure SQL Database's deadlock telemetry, which a database user can read (system_health it can't).
fn telemetry_sql(hours: u32) -> String {
    format!(
        "SELECT TOP ({READ_LIMIT}) CONVERT(varchar(33), x.timestamp_utc, 127) AS at, CAST(x.event_data AS nvarchar(max)) AS event_xml
           FROM sys.fn_xe_telemetry_blob_target_read_file('dl', NULL, NULL, NULL) x
          WHERE x.object_name = N'database_xml_deadlock_report' AND x.timestamp_utc >= DATEADD(hour, -{hours}, SYSUTCDATETIME())
          ORDER BY x.timestamp_utc DESC"
    )
}

const AZURE_SQL_DB: f64 = 5.0;
const TELEMETRY: &str = "Azure SQL deadlock telemetry";

/// Deadlocks in the last `since_hours`. On SQL Server from system_health: its event files first (they keep days), the
/// in-memory ring buffer when files can't be read. On Azure SQL Database from its deadlock telemetry. Permission
/// problems come back as a note, not an error.
pub(crate) async fn recent(client: &mut MsClient, since_hours: f64) -> tiberius::Result<DeadlockReport> {
    let hours = since_hours.clamp(1.0, 24.0 * 30.0).round() as u32;
    let mut report = DeadlockReport { since_hours: hours as f64, ..Default::default() };
    let server =
        query_one(client, "SELECT CAST(SERVERPROPERTY('EngineEdition') AS int) AS edition, DB_NAME() AS db").await?;
    let database = server.string("db");
    if server.f64("edition") == Some(AZURE_SQL_DB) {
        report.source = TELEMETRY.into();
        match query(client, &telemetry_sql(hours)).await {
            Ok(rows) => {
                for row in rows {
                    let Some(xml) = row.str("event_xml") else { continue };
                    let at = row.str("at").map(|t| normalize_time(&t));
                    report.deadlocks.extend(telemetry_deadlocks(&xml, &database, at));
                }
                report.deadlocks.sort_by(|a, b| b.at.cmp(&a.at));
            }
            Err(e) => {
                report.source = "Azure SQL Database".into();
                report.note = Some(format!(
                    "Culprit couldn't read Azure SQL Database's deadlock telemetry ({e}). Deadlocks are also in Azure \
                     Monitor (diagnostic setting \"Deadlocks\"), or paste a deadlock graph (.xdl) to have it explained."
                ));
            }
        }
        return Ok(report);
    }
    // Files keep days of history but are written with a delay; the ring buffer holds the latest events in memory.
    // Read both and merge, so a deadlock from a minute ago shows up too.
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let mut sources: Vec<&str> = Vec::new();
    if let Ok(rows) = query(client, &file_sql(hours)).await {
        sources.push("event files");
        for row in rows {
            let at = row.str("at").map(|t| normalize_time(&t));
            if let Some(xml) = row.str("event_xml") {
                report.deadlocks.extend(parse_with_time(&xml, "system_health", at).unwrap_or_default());
            }
        }
    }
    if let Ok(rows) = query(client, RING_SQL).await {
        sources.push("ring buffer");
        for row in rows {
            let Some(xml) = row.str("target_xml") else { continue };
            for d in parse_deadlock_xml(&xml, "system_health").unwrap_or_default() {
                let recent =
                    d.at.as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .is_none_or(|t| t >= cutoff);
                if recent && !report.deadlocks.iter().any(|known| known.id == d.id) {
                    report.deadlocks.push(d);
                }
            }
        }
    }
    // system_health is the instance's: other databases' deadlocks are theirs, not this database's.
    report.deadlocks.retain(|d| involves(d, &database));
    report.deadlocks.sort_by(|a, b| b.at.cmp(&a.at));
    report.deadlocks.truncate(READ_LIMIT);
    report.source =
        if sources.is_empty() { "system_health".into() } else { format!("system_health {}", sources.join(" and ")) };
    if sources.is_empty() {
        report.note = Some(
            "Reading deadlocks needs VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE on SQL Server 2022). \
             Paste a deadlock graph (.xdl) to have it explained."
                .into(),
        );
    } else if !sources.contains(&"event files") {
        report.note =
            Some("Read from system_health's in-memory buffer, which only keeps the most recent events.".into());
    }
    Ok(report)
}

/// Whether a deadlock from the instance-wide system_health session involves `database`: one of its processes ran in
/// it. A graph that names no database for any process can't be placed and is kept.
fn involves(d: &Deadlock, database: &str) -> bool {
    let mut named = d.processes.iter().filter_map(|p| p.database.as_deref()).peekable();
    named.peek().is_none() || named.any(|db| db.eq_ignore_ascii_case(database))
}

/// `2026-10-11T12:05:01.1230000` (UTC, no zone) → `2026-10-11T12:05:01Z`.
fn normalize_time(text: &str) -> String {
    let t = text.trim().trim_end_matches('Z');
    let t = t.split('.').next().unwrap_or(t);
    format!("{t}Z")
}

/// The deadlocks in one telemetry event, unless it names another database: the telemetry may be the logical server's,
/// and deadlock graphs there name databases by their physical name, so the event's `database_name` is what tells.
fn telemetry_deadlocks(xml: &str, database: &str, at: Option<String>) -> Vec<Deadlock> {
    let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
    let Ok(doc) = Document::parse_with_options(xml, options) else { return Vec::new() };
    let other = doc
        .descendants()
        .find(|n| n.has_tag_name("data") && n.attribute("name") == Some("database_name"))
        .and_then(|d| child(d, "value"))
        .and_then(text_of)
        .is_some_and(|name| !name.eq_ignore_ascii_case(database));
    if other {
        return Vec::new();
    }
    let mut found = parse_with_time(xml, TELEMETRY, at).unwrap_or_default();
    // A fix that names the database (ALTER DATABASE) needs the name people use.
    for p in found.iter_mut().flat_map(|d| d.processes.iter_mut()) {
        p.database = Some(database.to_string());
    }
    found
}

fn parse_with_time(xml: &str, source: &str, at: Option<String>) -> Result<Vec<Deadlock>> {
    let mut found = parse_deadlock_xml(xml, source)?;
    for d in &mut found {
        if d.at.is_none() {
            d.at = at.clone();
        }
    }
    Ok(found)
}

/// Every deadlock in `xml`: a saved .xdl (`<deadlock-list>` or `<deadlock>`), a system_health event, or a ring buffer.
pub fn parse_deadlock_xml(xml: &str, source: &str) -> Result<Vec<Deadlock>> {
    let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
    let doc = Document::parse_with_options(xml.trim_start_matches('\u{feff}'), options)
        .map_err(|e| Error::invalid(format!("This isn't a deadlock graph Culprit can read: {e}")))?;
    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("deadlock")) {
        // Ring buffers hold every kind of event; only deadlock reports carry a <deadlock>.
        let at = node
            .ancestors()
            .find(|a| a.has_tag_name("event"))
            .and_then(|e| e.attribute("timestamp"))
            .map(normalize_time);
        if let Some(d) = deadlock(node, source, at) {
            out.push(d);
        }
    }
    if out.is_empty() && !doc.descendants().any(|n| n.has_tag_name("RingBufferTarget") || n.has_tag_name("event")) {
        return Err(Error::invalid("No <deadlock> element found")
            .with_hint("Save the deadlock graph from SSMS or system_health as .xdl XML."));
    }
    Ok(out)
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| c.has_tag_name(name))
}

fn text_of(node: Node) -> Option<String> {
    let text: String = node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
    let text = text.trim();
    (!text.is_empty() && !text.eq_ignore_ascii_case("unknown")).then(|| clip(text))
}

fn clip(text: &str) -> String {
    const MAX: usize = 4000;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}

/// `db.schema.table` → `schema.table`.
fn object_of(name: &str) -> String {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() >= 2 { parts[parts.len() - 2..].join(".") } else { name.to_string() }
}

fn attr(node: Node, name: &str) -> Option<String> {
    node.attribute(name).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

fn deadlock(node: Node, source: &str, at: Option<String>) -> Option<Deadlock> {
    let mut victims: Vec<String> = child(node, "victim-list")
        .map(|v| v.children().filter(|c| c.has_tag_name("victimProcess")).filter_map(|c| attr(c, "id")).collect())
        .unwrap_or_default();
    if let Some(v) = attr(node, "victim") {
        victims.push(v);
    }
    let processes: Vec<DeadlockProcess> = child(node, "process-list")?
        .children()
        .filter(|c| c.has_tag_name("process"))
        .map(|p| {
            let id = attr(p, "id").unwrap_or_default();
            let frame = child(p, "executionStack").and_then(|s| {
                s.children().filter(|f| f.has_tag_name("frame")).find_map(|f| text_of(f).map(|t| (f, t)))
            });
            let procedure = frame
                .as_ref()
                .and_then(|(f, _)| attr(*f, "procname"))
                .filter(|n| !matches!(n.to_ascii_lowercase().as_str(), "adhoc" | "unknown"))
                .map(|n| object_of(&n));
            DeadlockProcess {
                victim: victims.contains(&id),
                session: attr(p, "spid"),
                statement: frame.map(|(_, t)| t).or_else(|| child(p, "inputbuf").and_then(text_of)),
                procedure,
                login: attr(p, "loginname"),
                host: attr(p, "hostname"),
                application: attr(p, "clientapp"),
                database: attr(p, "currentdbname"),
                isolation: attr(p, "isolationlevel").map(|i| i.split(" (").next().unwrap_or(&i).to_string()),
                wait_mode: attr(p, "lockMode"),
                log_used: attr(p, "logused").and_then(|v| v.parse().ok()),
                id,
            }
        })
        .collect();
    if processes.is_empty() {
        return None;
    }
    let resources: Vec<DeadlockResource> = child(node, "resource-list")
        .map(|list| {
            list.children()
                .filter(|c| c.is_element())
                .enumerate()
                .map(|(i, r)| {
                    let holders = |list: &str, item: &str| -> Vec<LockHolder> {
                        child(r, list)
                            .map(|l| {
                                l.children()
                                    .filter(|c| c.has_tag_name(item))
                                    .filter_map(|c| Some(LockHolder { process: attr(c, "id")?, mode: attr(c, "mode") }))
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    DeadlockResource {
                        id: attr(r, "id").unwrap_or_else(|| format!("{}{i}", r.tag_name().name())),
                        kind: r.tag_name().name().to_string(),
                        object: attr(r, "objectname").map(|o| object_of(&o)),
                        index: attr(r, "indexname"),
                        holders: holders("owner-list", "owner"),
                        waiters: holders("waiter-list", "waiter"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let mut d = Deadlock::new(source, at, processes, resources);
    classify(Engine::Mssql, &mut d);
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::DeadlockPattern;

    pub(crate) const KEY_LOOKUP_XDL: &str = r#"<deadlock-list><deadlock victim="process61">
 <process-list>
  <process id="process61" spid="61" lockMode="S" isolationlevel="read committed (2)" currentdbname="shop" loginname="app" hostname="web-1" clientapp="orders-api" logused="0">
   <executionStack><frame procname="adhoc" line="1">SELECT id, name, segment FROM shop.customers WHERE email = @P1</frame></executionStack>
   <inputbuf>(@P1 nvarchar(200))SELECT id, name, segment FROM shop.customers WHERE email = @P1</inputbuf>
  </process>
  <process id="process64" spid="64" lockMode="X" isolationlevel="read committed (2)" currentdbname="shop" loginname="app" hostname="worker-2" clientapp="billing" logused="1240">
   <executionStack><frame procname="adhoc" line="1">UPDATE shop.customers SET email = @P1 WHERE id = @P2</frame></executionStack>
   <inputbuf />
  </process>
 </process-list>
 <resource-list>
  <keylock hobtid="1" dbid="5" objectname="shop.shop.customers" indexname="ix_customers_email" id="lock1" mode="X">
   <owner-list><owner id="process64" mode="X"/></owner-list>
   <waiter-list><waiter id="process61" mode="S" requestType="wait"/></waiter-list>
  </keylock>
  <keylock hobtid="2" dbid="5" objectname="shop.shop.customers" indexname="PK_customers" id="lock2" mode="S">
   <owner-list><owner id="process61" mode="S"/></owner-list>
   <waiter-list><waiter id="process64" mode="X" requestType="wait"/></waiter-list>
  </keylock>
 </resource-list>
</deadlock></deadlock-list>"#;

    #[test]
    fn a_saved_xdl_is_parsed_and_explained() {
        let found = parse_deadlock_xml(KEY_LOOKUP_XDL, "pasted").unwrap();
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.pattern, DeadlockPattern::KeyLookup);
        assert_eq!(d.objects, vec!["shop.customers".to_string()]);
        let victim = d.victim().unwrap();
        assert_eq!((victim.session.as_deref(), victim.application.as_deref()), (Some("61"), Some("orders-api")));
        assert_eq!(victim.isolation.as_deref(), Some("read committed"));
        assert!(victim.statement.as_deref().unwrap().starts_with("SELECT id, name"));
        assert_eq!(d.resources[0].holders[0].mode.as_deref(), Some("X"));
    }

    // system_health records every database's deadlocks; a scan of one database keeps only its own.
    #[test]
    fn instance_wide_deadlocks_are_kept_only_for_this_database() {
        let found = parse_deadlock_xml(KEY_LOOKUP_XDL, "system_health").unwrap();
        assert!(involves(&found[0], "SHOP"), "both processes ran in shop");
        assert!(!involves(&found[0], "billing"), "another database's deadlock");
        let unnamed = parse_deadlock_xml(&KEY_LOOKUP_XDL.replace(r#" currentdbname="shop""#, ""), "pasted").unwrap();
        assert!(involves(&unnamed[0], "billing"), "a graph without databases can't be placed, so it stays");
    }

    #[test]
    fn system_health_events_carry_their_time_and_dtds_are_refused() {
        let event = format!(
            r#"<event name="xml_deadlock_report" timestamp="2026-10-11T12:05:01.123Z"><data name="xml_report"><value>{}</value></data></event>"#,
            KEY_LOOKUP_XDL.replace("<deadlock-list>", "").replace("</deadlock-list>", "")
        );
        let found = parse_deadlock_xml(&event, "system_health").unwrap();
        assert_eq!(found[0].at.as_deref(), Some("2026-10-11T12:05:01Z"));
        let evil =
            r#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY e SYSTEM "file:///etc/passwd">]><deadlock>&e;</deadlock>"#;
        assert!(parse_deadlock_xml(evil, "pasted").is_err());
        assert!(parse_deadlock_xml("<nothing/>", "pasted").is_err());
    }

    /// One event from Azure SQL Database's deadlock telemetry, as `event_data` holds it.
    fn telemetry_event(database: &str) -> String {
        let graph = KEY_LOOKUP_XDL
            .replace("<deadlock-list>", "")
            .replace("</deadlock-list>", "")
            // The graph names the database by its physical name there.
            .replace(r#"currentdbname="shop""#, r#"currentdbname="5f0c1e9a-7d2b-4c66-9e1f-3a8b2d4c6e10""#);
        format!(
            r#"<event name="database_xml_deadlock_report" package="sqlserver" timestamp="2026-10-11T12:05:01.123Z"><data name="xml_report"><type name="xml" package="package0"/><value>{graph}</value></data><data name="database_name"><type name="unicode_string" package="package0"/><value>{database}</value></data></event>"#
        )
    }

    #[test]
    fn azure_sql_telemetry_events_are_read_for_this_database_only() {
        let found = telemetry_deadlocks(&telemetry_event("shop"), "Shop", Some("2026-10-11T12:05:01Z".into()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pattern, DeadlockPattern::KeyLookup);
        assert_eq!(found[0].at.as_deref(), Some("2026-10-11T12:05:01Z"));
        assert_eq!(found[0].source, TELEMETRY);
        assert_eq!(found[0].objects, vec!["shop.customers".to_string()]);
        assert!(found[0].processes.iter().all(|p| p.database.as_deref() == Some("Shop")));
        // Another database on the same logical server.
        assert!(telemetry_deadlocks(&telemetry_event("billing"), "shop", None).is_empty());
        assert!(telemetry_deadlocks("not xml", "shop", None).is_empty());
    }
}
