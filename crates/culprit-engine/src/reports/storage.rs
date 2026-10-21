//! Private, versioned disk encoding. Keep the prepared snapshot once and lossless differences for the two retained
//! observations. Native plans, query text and unchanged catalogs are not repeated in the same file.

use culprit_core::{ChangeEvent, Report};
use serde::{Deserialize, Serialize, de::Error};
use serde_json::Value;

const KEY: &str = "_culprit_store";

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    input: Difference,
    collected: Option<Difference>,
    recorded_changes: Vec<ChangeEvent>,
}

#[derive(Serialize, Deserialize)]
struct Difference {
    changes: Vec<Change>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
enum Step {
    Key(String),
    Index(usize),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Change {
    Set { path: Vec<Step>, value: Value },
    Remove { path: Vec<Step> },
    Resize { path: Vec<Step>, length: usize },
}

// Flat paths keep the encoding's JSON nesting bounded even for deeply nested execution plans.
fn difference(before: &Value, after: &Value) -> Difference {
    fn visit(before: &Value, after: &Value, path: &mut Vec<Step>, changes: &mut Vec<Change>) {
        if before == after {
            return;
        }
        match (before, after) {
            (Value::Object(before), Value::Object(after)) => {
                for (key, value) in after {
                    path.push(Step::Key(key.clone()));
                    match before.get(key) {
                        Some(old) => visit(old, value, path, changes),
                        None => changes.push(Change::Set { path: path.clone(), value: value.clone() }),
                    }
                    path.pop();
                }
                for key in before.keys().filter(|key| !after.contains_key(*key)) {
                    path.push(Step::Key(key.clone()));
                    changes.push(Change::Remove { path: path.clone() });
                    path.pop();
                }
            }
            (Value::Array(before), Value::Array(after)) => {
                if before.len() != after.len() {
                    changes.push(Change::Resize { path: path.clone(), length: after.len() });
                }
                for (i, value) in after.iter().enumerate() {
                    path.push(Step::Index(i));
                    match before.get(i) {
                        Some(old) => visit(old, value, path, changes),
                        None => changes.push(Change::Set { path: path.clone(), value: value.clone() }),
                    }
                    path.pop();
                }
            }
            _ => changes.push(Change::Set { path: path.clone(), value: after.clone() }),
        }
    }
    let mut changes = Vec::new();
    visit(before, after, &mut Vec::new(), &mut changes);
    Difference { changes }
}

fn target<'a>(mut value: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    for step in path {
        value = match step {
            Step::Key(key) => value.as_object_mut()?.get_mut(key)?,
            Step::Index(i) => value.as_array_mut()?.get_mut(*i)?,
        };
    }
    Some(value)
}

fn apply(value: &mut Value, diff: Difference) -> serde_json::Result<()> {
    let invalid = || serde_json::Error::custom("Invalid retained evidence difference");
    let growth_limit = diff.changes.len();
    for change in diff.changes {
        match change {
            Change::Set { path, value: after } => {
                let Some((last, parent)) = path.split_last() else {
                    *value = after;
                    continue;
                };
                let parent = target(value, parent).ok_or_else(invalid)?;
                match last {
                    Step::Key(key) => {
                        parent.as_object_mut().ok_or_else(invalid)?.insert(key.clone(), after);
                    }
                    Step::Index(i) => *parent.as_array_mut().and_then(|a| a.get_mut(*i)).ok_or_else(invalid)? = after,
                }
            }
            Change::Remove { path } => {
                let Some((Step::Key(key), parent)) = path.split_last() else {
                    return Err(invalid());
                };
                target(value, parent).and_then(Value::as_object_mut).ok_or_else(invalid)?.remove(key);
            }
            Change::Resize { path, length } => {
                let array = target(value, &path).and_then(Value::as_array_mut).ok_or_else(invalid)?;
                // A tiny malformed file cannot request an unbounded allocation. Added slots require set operations.
                if length > array.len().saturating_add(growth_limit) {
                    return Err(invalid());
                }
                array.resize(length, Value::Null);
            }
        }
    }
    Ok(())
}

pub(super) fn encode(report: &Report) -> serde_json::Result<Vec<u8>> {
    let mut value = serde_json::to_value(super::display(report))?;
    if let Some(e) = &report.evidence {
        let input = serde_json::to_value(&e.input)?;
        let collected =
            e.collected.as_ref().map(|s| serde_json::to_value(s).map(|s| difference(&input, &s))).transpose()?;
        let retained = Stored {
            version: 1,
            input: difference(&value["snapshot"], &input),
            collected,
            recorded_changes: e.recorded_changes.clone(),
        };
        value[KEY] = serde_json::to_value(retained)?;
    }
    serde_json::to_vec(&value)
}

pub(super) fn expand(value: &mut Value) -> serde_json::Result<()> {
    let Some(stored) = value.as_object_mut().and_then(|o| o.remove(KEY)) else {
        return Ok(());
    };
    let stored: Stored = serde_json::from_value(stored)?;
    if stored.version != 1 {
        return Err(serde_json::Error::custom("Unsupported saved report encoding"));
    }
    let mut input = value.get("snapshot").cloned().ok_or_else(|| serde_json::Error::custom("No saved snapshot"))?;
    apply(&mut input, stored.input)?;
    let collected = if let Some(diff) = stored.collected {
        let mut collected = input.clone();
        apply(&mut collected, diff)?;
        collected
    } else {
        Value::Null
    };
    value["evidence"] =
        serde_json::json!({"input": input, "collected": collected, "recorded_changes": stored.recorded_changes});
    Ok(())
}

pub(super) fn decode(bytes: &[u8]) -> serde_json::Result<Report> {
    let mut value = serde_json::from_slice(bytes)?;
    expand(&mut value)?;
    let mut report = serde_json::from_value(value)?;
    share_native_text(&mut report);
    Ok(report)
}

pub(super) fn share_native_text(report: &mut Report) {
    use std::{collections::HashSet, sync::Arc};
    let mut texts: HashSet<Arc<str>> = HashSet::new();
    let mut share = |snapshot: &mut culprit_core::Snapshot| {
        for q in snapshot.workload.iter_mut().flat_map(|w| &mut w.queries) {
            if let Some(raw) = q.plan.as_mut().and_then(|p| p.raw.as_mut()) {
                if let Some(shared) = texts.get(raw.as_ref()) {
                    *raw = shared.clone();
                } else {
                    texts.insert(raw.clone());
                }
            }
        }
    };
    share(&mut report.snapshot);
    if let Some(evidence) = &mut report.evidence {
        share(&mut evidence.input);
        if let Some(collected) = &mut evidence.collected {
            share(collected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn differences_preserve_null_removal_reordering_and_array_length_changes() {
        let values = [
            serde_json::json!({"a": null, "b": [1, "é", {"x": true}]}),
            serde_json::json!({"b": [null, {"z": []}], "c": 4}),
            serde_json::json!({"a": [], "b": [2, 3, 4, 5, 6]}),
        ];
        for before in &values {
            for after in &values {
                let mut result = before.clone();
                apply(&mut result, difference(before, after)).unwrap();
                assert_eq!(&result, after);
            }
        }
        assert!(
            apply(
                &mut serde_json::json!([]),
                Difference { changes: vec![Change::Resize { path: Vec::new(), length: usize::MAX }] }
            )
            .is_err()
        );
    }

    #[test]
    fn deep_plan_differences_do_not_deepen_the_serialized_document() {
        let mut before = serde_json::json!(0);
        let mut after = serde_json::json!(1);
        for _ in 0..80 {
            before = serde_json::json!({"child": before});
            after = serde_json::json!({"child": after});
        }
        let bytes = serde_json::to_vec(&difference(&before, &after)).unwrap();
        let restored: Difference = serde_json::from_slice(&bytes).unwrap();
        apply(&mut before, restored).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn compact_reports_preserve_all_evidence_without_repeating_native_plans() {
        let mut snapshot: culprit_core::Snapshot =
            serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let native = "retained-native-plan-canary".repeat(20_000);
        snapshot.workload.as_mut().unwrap().queries.iter_mut().find_map(|q| q.plan.as_mut()).unwrap().raw =
            Some(native.clone().into());
        let mut report = culprit_checks::analyze(snapshot.clone());
        let evidence = report.evidence.as_mut().unwrap();
        evidence.collected = Some(Box::new(snapshot));
        evidence.input.indexes[0].counters_since = Some("2026-10-01T00:00:00Z".into());
        let raw = |s: &culprit_core::Snapshot| {
            s.workload
                .as_ref()
                .unwrap()
                .queries
                .iter()
                .find_map(|q| q.plan.as_ref().and_then(|p| p.raw.clone()))
                .unwrap()
        };
        assert!(std::sync::Arc::ptr_eq(&raw(&report.snapshot), &raw(&report.evidence.as_ref().unwrap().input)));
        let full = serde_json::to_vec(&report).unwrap();
        let compact = encode(&report).unwrap();
        let decoded = decode(&compact).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &raw(&decoded.snapshot),
            &raw(decoded.evidence.as_ref().unwrap().collected.as_ref().unwrap())
        ));
        assert!(compact.len() < full.len() * 3 / 5, "{} vs {}", compact.len(), full.len());
        assert_eq!(String::from_utf8_lossy(&compact).matches(&native).count(), 1);
        // Output-only plan projections are deliberately discarded by both encodings when loaded.
        assert_eq!(
            serde_json::to_value(decode(&compact).unwrap()).unwrap(),
            serde_json::to_value(decode(&full).unwrap()).unwrap()
        );
        let mut expected = serde_json::to_value(&report).unwrap();
        expected.as_object_mut().unwrap().remove("evidence");
        assert_eq!(serde_json::to_value(super::super::display(&report)).unwrap(), expected);
    }
}
