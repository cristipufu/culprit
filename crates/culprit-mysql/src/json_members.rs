//! EXPLAIN JSON read member by member. Before MariaDB 11, `EXPLAIN FORMAT=JSON` writes each table of a join as another
//! `"table"` (or `"block-nl-join"`) member of the same object, and a JSON map keeps only the last of repeated members,
//! so every table but the last would vanish. This reads every member in order and folds an object's join members into
//! the `"nested_loop"` array MariaDB 11 writes, so one plan reader handles both. MySQL never repeats a member.

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// Members that are one table of a join, in join order.
const JOIN_MEMBERS: [&str; 2] = ["table", "block-nl-join"];

/// Parse EXPLAIN JSON, keeping repeated join members as a `nested_loop`.
pub fn parse(json: &str) -> serde_json::Result<Value> {
    let raw: Raw = serde_json::from_str(json)?;
    Ok(fold(raw))
}

/// JSON as written: objects keep every member, in order, duplicates included.
enum Raw {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Raw>),
    Object(Vec<(String, Raw)>),
}

fn fold(raw: Raw) -> Value {
    match raw {
        Raw::Null => Value::Null,
        Raw::Bool(b) => Value::Bool(b),
        Raw::Number(n) => Value::Number(n),
        Raw::String(s) => Value::String(s),
        Raw::Array(items) => Value::Array(items.into_iter().map(fold).collect()),
        Raw::Object(members) => {
            let joined = members.iter().filter(|(key, _)| JOIN_MEMBERS.contains(&key.as_str())).count();
            let mut map = Map::new();
            if joined < 2 {
                for (key, value) in members {
                    map.insert(key, fold(value));
                }
                return Value::Object(map);
            }
            let mut tables = Vec::with_capacity(joined);
            for (key, value) in members {
                if JOIN_MEMBERS.contains(&key.as_str()) {
                    let mut item = Map::new();
                    item.insert(key, fold(value));
                    tables.push(Value::Object(item));
                } else {
                    map.insert(key, fold(value));
                }
            }
            map.insert("nested_loop".into(), Value::Array(tables));
            Value::Object(map)
        }
    }
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RawVisitor)
    }
}

struct RawVisitor;

impl<'de> Visitor<'de> for RawVisitor {
    type Value = Raw;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Raw, E> {
        Ok(Raw::Null)
    }

    fn visit_none<E>(self) -> Result<Raw, E> {
        Ok(Raw::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Raw, E> {
        Ok(Raw::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Raw, E> {
        Ok(Raw::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Raw, E> {
        Ok(Raw::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Raw, E> {
        Ok(Number::from_f64(v).map(Raw::Number).unwrap_or(Raw::Null))
    }

    fn visit_str<E>(self, v: &str) -> Result<Raw, E> {
        Ok(Raw::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Raw, E> {
        Ok(Raw::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Raw, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Raw::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
        let mut members = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Raw>()? {
            members.push((key, value));
        }
        Ok(Raw::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_tables_become_a_nested_loop_in_join_order() {
        // MariaDB 10.5: a join written as repeated `table` members, with a join buffer between them.
        let json = r#"{"query_block": {"select_id": 1,
            "table": {"table_name": "o", "access_type": "range", "rows": 4},
            "block-nl-join": {"table": {"table_name": "p", "access_type": "ALL", "rows": 19658}, "join_type": "BNL"},
            "table": {"table_name": "c", "access_type": "ref", "rows": 24870}}}"#;
        let value = parse(json).unwrap();
        let block = &value["query_block"];
        assert_eq!(block["select_id"], 1);
        assert!(block.get("table").is_none());
        let names: Vec<String> = block["nested_loop"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let table = item.get("table").or_else(|| item["block-nl-join"].get("table")).unwrap();
                table["table_name"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(names, ["o", "p", "c"]);
    }

    #[test]
    fn single_tables_and_mysql_plans_are_unchanged() {
        let json = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "4.80"},
            "nested_loop": [{"table": {"table_name": "oi"}}, {"table": {"table_name": "p"}}]}}"#;
        assert_eq!(parse(json).unwrap(), serde_json::from_str::<Value>(json).unwrap());
        let single =
            r#"{"query_block": {"table": {"table_name": "t", "rows": 1.5, "filtered": 100, "x": null, "ok": true}}}"#;
        assert_eq!(parse(single).unwrap(), serde_json::from_str::<Value>(single).unwrap());
    }
}
