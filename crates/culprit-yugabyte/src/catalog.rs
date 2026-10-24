use crate::{probe::Probe, sources};
use culprit_core::{
    ColumnInfo, ConstraintIssue, ConstraintKind, Coverage, ForeignKey, IndexStat, Result, Setting, Snapshot, TableStat,
    signals::{DocdbIndex, DocdbTable, TableStorage},
};
use culprit_postgres::wire::{Row, query, query_bounded};
use serde_json::json;
use std::collections::BTreeMap;
use tokio_postgres::Client;

const USER_RELATIONS: &str = "n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp_%'";
fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| culprit_core::Error::invalid("YSQL returned incomplete catalog metadata"))
}
fn array(row: &Row, key: &str) -> Vec<String> {
    row.text(key).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
pub(crate) async fn tables(client: &Client) -> Result<Vec<TableStat>> {
    let rows = query(
        client,
        &format!(
            "SELECT n.nspname AS schema, c.relname AS name,
        CASE WHEN c.reltuples >= 0 THEN c.reltuples::float8 END AS rows,
        EXISTS(SELECT 1 FROM pg_index i WHERE i.indrelid=c.oid AND i.indisprimary) AS pk,
        (SELECT pn.nspname || '.' || p.relname FROM pg_inherits inh JOIN pg_class p ON p.oid=inh.inhparent
         JOIN pg_namespace pn ON pn.oid=p.relnamespace WHERE inh.inhrelid=c.oid LIMIT 1) AS parent,
        c.relkind::text AS kind FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        WHERE c.relkind IN ('r','p','m') AND {USER_RELATIONS} ORDER BY n.nspname,c.relname"
        ),
    )
    .await?;
    rows.iter().map(|r|decode(json!({"schema":r.string("schema"),"name":r.string("name"),"rows":r.number("rows"),
        "has_primary_key":r.flag("pk"),"kind":if r.string("kind")=="p" {"partitioned"} else {"docdb"},"partition_of":r.text("parent")}))).collect()
}
pub(crate) async fn indexes(client: &Client, _probe: &Probe) -> Result<Vec<IndexStat>> {
    let rows = query(client,&format!("SELECT n.nspname AS schema,t.relname AS table,i.relname AS name,am.amname AS kind,
        x.indisprimary AS pk,x.indisunique AS unique,x.indisready AS ready,x.indisvalid AS usable,
        pg_get_indexdef(i.oid) AS definition,pg_get_expr(x.indpred,x.indrelid) AS filter,
        (SELECT pn.nspname || '.' || p.relname FROM pg_inherits inh JOIN pg_class p ON p.oid=inh.inhparent
         JOIN pg_namespace pn ON pn.oid=p.relnamespace WHERE inh.inhrelid=i.oid LIMIT 1) AS partition_of,
        array_to_json(ARRAY(SELECT COALESCE(a.attname,pg_get_indexdef(i.oid,k,true)) || CASE WHEN (x.indoption[k-1] & 1)=1 THEN ' DESC' ELSE '' END
          FROM generate_series(1,x.indnkeyatts) k LEFT JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=x.indkey[k-1] ORDER BY k))::text AS keys,
        array_to_json(ARRAY(SELECT concat_ws(' ',
          CASE WHEN NOT opc.opcdefault THEN opc.opcname END,
          CASE WHEN x.indcollation[k-1] <> 0 AND x.indcollation[k-1] IS DISTINCT FROM COALESCE(
            (SELECT a.attcollation FROM pg_attribute a WHERE a.attrelid=x.indrelid AND a.attnum=x.indkey[k-1]),100)
            THEN 'COLLATE ' || quote_ident(coll.collname) END)
          FROM generate_series(1,x.indnkeyatts) k JOIN pg_opclass opc ON opc.oid=x.indclass[k-1]
          LEFT JOIN pg_collation coll ON coll.oid=x.indcollation[k-1] ORDER BY k))::text AS opclasses,
        array_to_json(ARRAY(SELECT a.attname FROM generate_series(x.indnkeyatts+1,x.indnatts) k
          JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=x.indkey[k-1] ORDER BY k))::text AS included
        FROM pg_index x JOIN pg_class i ON i.oid=x.indexrelid JOIN pg_class t ON t.oid=x.indrelid
        JOIN pg_namespace n ON n.oid=t.relnamespace JOIN pg_am am ON am.oid=i.relam WHERE i.relkind='i' AND {USER_RELATIONS} ORDER BY 1,2,3")).await?;
    rows.iter().map(|r|decode(json!({"schema":r.string("schema"),"table":r.string("table"),"name":r.string("name"),"kind":r.string("kind"),
        "columns":array(r,"keys"),"opclasses":array(r,"opclasses"),"include":array(r,"included"),"filter":r.text("filter"),"is_primary_key":r.flag("pk"),
        "is_unique":r.flag("unique"),"is_clustered":r.flag("pk"),"is_disabled":!r.flag("usable"),"maintained":r.flag("ready"),"definition":r.text("definition"),"partition_of":r.text("partition_of")}))).collect()
}
pub(crate) async fn details(client: &Client, snapshot: &mut Snapshot, coverage: &mut Coverage) {
    let props = query(
        client,
        &format!(
            "SELECT n.nspname AS schema,c.relname AS name,p.* FROM pg_class c
        JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL yb_table_properties(c.oid) p
        WHERE c.relkind IN ('r','m','i') AND {USER_RELATIONS}"
        ),
    )
    .await;
    let properties: BTreeMap<_, _> = coverage
        .read_rows(&sources::STORAGE, props)
        .unwrap_or_default()
        .into_iter()
        .map(|r| ((r.string("schema"), r.string("name")), r))
        .collect();
    let mut table_properties = BTreeMap::new();
    for table in &mut snapshot.tables {
        let key = (table.schema.clone(), table.name.clone());
        let Some(row) = properties.get(&key) else { continue };
        let Some(colocated) = row.text("is_colocated").map(|_| row.flag("is_colocated")) else { continue };
        let native = DocdbTable {
            tablets: row.string("num_tablets").parse().ok(),
            hash_key_columns: row.string("num_hash_key_columns").parse().ok(),
            colocated,
            tablegroup: row.text("tablegroup_oid"),
        };
        table_properties.insert(key, native.clone());
        table.storage = Some(TableStorage { docdb: Some(native), ..Default::default() });
    }
    for index in &mut snapshot.indexes {
        // A primary key is stored in the base LSM table; its index OID has no independent table properties.
        let (hash_keys, tablets) = if index.is_primary_key {
            let Some(base) = table_properties.get(&(index.schema.clone(), index.table.clone())) else { continue };
            (base.hash_key_columns, base.tablets)
        } else {
            let Some(row) = properties.get(&(index.schema.clone(), index.name.clone())) else { continue };
            (row.string("num_hash_key_columns").parse().ok(), row.string("num_tablets").parse().ok())
        };
        if let Some(hash_keys) = hash_keys.filter(|n| *n as usize <= index.columns.len()) {
            index.docdb = Some(DocdbIndex {
                hash_columns: index.key_names().into_iter().take(hash_keys as usize).map(str::to_string).collect(),
                tablets,
            });
        }
    }
    let sizes = query(
        client,
        &format!(
            "SELECT n.nspname AS schema,c.relname AS name,c.relkind::text AS kind,
        pg_table_size(c.oid)::float8 AS data,
        CASE WHEN c.relkind <> 'i' THEN pg_indexes_size(c.oid)::float8 END AS indexes,
        CASE WHEN c.relkind <> 'i' THEN pg_total_relation_size(c.oid)::float8 END AS total
        FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        WHERE c.relkind IN ('r','m','i') AND {USER_RELATIONS}
          AND NOT EXISTS(SELECT 1 FROM pg_index x WHERE x.indexrelid=c.oid AND x.indisprimary)"
        ),
    )
    .await;
    let sizes: BTreeMap<_, _> = coverage
        .read_rows(&sources::SIZES, sizes)
        .unwrap_or_default()
        .into_iter()
        .map(|r| ((r.string("schema"), r.string("name")), r))
        .collect();
    for table in &mut snapshot.tables {
        if !table.storage.as_ref().and_then(|s| s.docdb.as_ref()).is_some_and(|d| !d.colocated) {
            continue;
        }
        if let Some(row) = sizes.get(&(table.schema.clone(), table.name.clone())) {
            table.data_bytes = row.number("data");
            table.index_bytes = row.number("indexes");
            table.total_bytes = row.number("total");
        }
    }
    for index in &mut snapshot.indexes {
        let key = (index.schema.clone(), index.name.clone());
        // Colocated tables/indexes share storage. Never charge their entire tablegroup to every logical object.
        let standalone =
            properties.get(&key).is_some_and(|r| r.text("is_colocated").is_some() && !r.flag("is_colocated"));
        if !index.is_primary_key && standalone {
            index.size_bytes = sizes.get(&key).and_then(|r| r.number("data"));
        }
    }
}
pub(crate) async fn columns(client: &Client, probe: &Probe) -> Result<Vec<ColumnInfo>> {
    let generated = if probe.compatibility >= 120000 { "a.attgenerated <> ''" } else { "false" };
    let rows=query(client,&format!("SELECT n.nspname AS schema,c.relname AS table,a.attname AS name,a.attnum AS ordinal,
        format_type(a.atttypid,a.atttypmod) AS type, NOT a.attnotnull AS nullable,pg_get_expr(d.adbin,d.adrelid) AS default,
        (a.attidentity <> '' OR {generated}) AS generated FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid
        JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum
        WHERE c.relkind IN ('r','p','m') AND a.attnum>0 AND NOT a.attisdropped AND {USER_RELATIONS} ORDER BY 1,2,a.attnum")).await?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: r.string("schema"),
            table: r.string("table"),
            name: r.string("name"),
            data_type: r.string("type"),
            nullable: r.flag("nullable"),
            default: r.text("default"),
            generated: r.flag("generated"),
            ordinal: r.string("ordinal").parse().unwrap_or(0),
        })
        .collect())
}
pub(crate) async fn foreign_keys(client: &Client) -> Result<Vec<ForeignKey>> {
    let rows=query(client,&format!("SELECT n.nspname AS schema,t.relname AS table,c.conname AS name,rn.nspname AS ref_schema,rt.relname AS ref_table,
        array_to_json(ARRAY(SELECT a.attname FROM unnest(c.conkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=k.num ORDER BY k.ord))::text AS columns,
        array_to_json(ARRAY(SELECT a.attname FROM unnest(c.confkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=rt.oid AND a.attnum=k.num ORDER BY k.ord))::text AS ref_columns,
        c.confdeltype::text AS delete_rule FROM pg_constraint c JOIN pg_class t ON t.oid=c.conrelid JOIN pg_namespace n ON n.oid=t.relnamespace
        JOIN pg_class rt ON rt.oid=c.confrelid JOIN pg_namespace rn ON rn.oid=rt.relnamespace WHERE c.contype='f' AND c.conparentid=0 AND {USER_RELATIONS}")).await?;
    Ok(rows
        .iter()
        .map(|r| ForeignKey {
            schema: r.string("schema"),
            table: r.string("table"),
            name: r.string("name"),
            columns: array(r, "columns"),
            ref_schema: r.string("ref_schema"),
            ref_table: r.string("ref_table"),
            ref_columns: array(r, "ref_columns"),
            delete_rule: match r.string("delete_rule").as_str() {
                "c" => Some("CASCADE".into()),
                "n" => Some("SET NULL".into()),
                "r" => Some("RESTRICT".into()),
                "d" => Some("SET DEFAULT".into()),
                _ => None,
            },
        })
        .collect())
}
pub(crate) async fn settings(client: &Client) -> Result<Vec<Setting>> {
    Ok(query(client,"SELECT name,setting,boot_val FROM pg_settings WHERE name LIKE 'yb_%' OR name IN ('work_mem','statement_timeout','lock_timeout','default_statistics_target','idle_in_transaction_session_timeout','plan_cache_mode')").await?.iter()
        .map(|r|Setting{name:r.string("name"),value:r.string("setting"),default_value:r.text("boot_val"),scope:"YSQL node".into()}).collect())
}

pub(crate) async fn constraints(client: &Client) -> Result<(Vec<ConstraintIssue>, bool)> {
    let (rows, truncated) = query_bounded(
        client,
        &format!(
            "SELECT n.nspname AS schema,t.relname AS table,c.conname AS name,c.contype::text AS kind
             FROM pg_constraint c JOIN pg_class t ON t.oid=c.conrelid
             JOIN pg_namespace n ON n.oid=t.relnamespace
             WHERE c.contype IN ('c','f') AND NOT c.convalidated AND c.conparentid=0
               AND {USER_RELATIONS} ORDER BY 1,2,3 LIMIT 10001"
        ),
        10_000,
    )
    .await?;
    let constraints = rows
        .iter()
        .map(|r| ConstraintIssue {
            schema: r.string("schema"),
            table: r.string("table"),
            name: r.string("name"),
            kind: if r.string("kind") == "f" { ConstraintKind::ForeignKey } else { ConstraintKind::Check },
            disabled: false,
            // Native validation evidence does not establish PostgreSQL constraint-exclusion behavior.
            inheritance_child: false,
        })
        .collect();
    Ok((constraints, truncated))
}
