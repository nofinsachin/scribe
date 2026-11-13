//! Decoder for the Kubernetes `FieldsV1` set encoding.
//!
//! A `FieldsV1` value is a trie encoded as JSON. Keys are one of:
//! * `.`        – the node itself is owned
//! * `f:<name>` – a struct field
//! * `k:<json>` – a list item identified by its merge keys
//! * `v:<json>` – a set member identified by its value
//! * `i:<n>`    – a list item identified by index
//!
//! An empty object marks an owned leaf.

use serde_json::Value;

/// Flatten a FieldsV1 trie into the sorted list of owned field paths,
/// e.g. `spec.containers[name=app].image`.
pub fn flatten(fields: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(fields, "", &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(node: &Value, path: &str, out: &mut Vec<String>) {
    let Some(map) = node.as_object() else { return };
    if map.is_empty() {
        if !path.is_empty() {
            out.push(path.to_string());
        }
        return;
    }
    for (key, child) in map {
        if key == "." {
            if !path.is_empty() {
                out.push(path.to_string());
            }
            continue;
        }
        let Some(seg) = segment(key, path.is_empty()) else {
            continue; // unknown key kind: ignore rather than fail
        };
        walk(child, &format!("{path}{seg}"), out);
    }
}

fn segment(key: &str, at_root: bool) -> Option<String> {
    if let Some(name) = key.strip_prefix("f:") {
        return Some(if at_root { name.to_string() } else { format!(".{name}") });
    }
    if let Some(k) = key.strip_prefix("k:") {
        return Some(match serde_json::from_str::<Value>(k) {
            Ok(Value::Object(m)) => {
                let parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}={}", bare(v))).collect();
                format!("[{}]", parts.join(","))
            }
            _ => format!("[{k}]"),
        });
    }
    if let Some(v) = key.strip_prefix("v:") {
        let val = serde_json::from_str::<Value>(v).map(|v| bare(&v)).unwrap_or_else(|_| v.to_string());
        return Some(format!("[={val}]"));
    }
    if let Some(i) = key.strip_prefix("i:") {
        return Some(format!("[{i}]"));
    }
    None
}

fn bare(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattens_nested_structs() {
        let v = json!({"f:metadata": {"f:labels": {".": {}, "f:app": {}}}, "f:spec": {"f:replicas": {}}});
        assert_eq!(flatten(&v), vec!["metadata.labels", "metadata.labels.app", "spec.replicas"]);
    }

    #[test]
    fn list_items_by_key() {
        let v = json!({"f:spec": {"f:containers": {
            "k:{\"name\":\"app\"}": {".": {}, "f:image": {}, "f:ports": {"k:{\"containerPort\":80,\"protocol\":\"TCP\"}": {".": {}}}}
        }}});
        assert_eq!(
            flatten(&v),
            vec![
                "spec.containers[name=app]",
                "spec.containers[name=app].image",
                "spec.containers[name=app].ports[containerPort=80,protocol=TCP]",
            ]
        );
    }

    #[test]
    fn set_values_and_indexes() {
        let v = json!({"f:metadata": {"f:finalizers": {"v:\"example.com/fin\"": {}}}, "f:x": {"i:2": {}}});
        assert_eq!(flatten(&v), vec!["metadata.finalizers[=example.com/fin]", "x[2]"]);
    }

    #[test]
    fn tolerates_junk() {
        assert!(flatten(&json!(null)).is_empty());
        assert!(flatten(&json!({"zzz": {}})).is_empty());
        assert!(flatten(&json!({})).is_empty());
    }
}
