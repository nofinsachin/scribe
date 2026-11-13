//! Domain model: an object, its owner references, and the field → actors graph.

use crate::{fieldsv1, timeutil};
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: String,
    pub controller: bool,
}

/// One writer of a field (a single managedFields entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub manager: String,
    pub operation: String,
    pub time: Option<String>,
    pub epoch: Option<i64>,
    pub subresource: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub path: String,
    /// Newest first. Never empty.
    pub actors: Vec<Actor>,
}

impl Field {
    pub fn last(&self) -> &Actor {
        &self.actors[0]
    }
}

#[derive(Debug, Clone)]
pub struct Object {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub uid: Option<String>,
    pub owners: Vec<OwnerRef>,
    pub fields: Vec<Field>,
    /// Number of managedFields entries (for display).
    pub entry_count: usize,
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

impl Object {
    pub fn from_value(v: &Value) -> Result<Object> {
        let Some(meta) = v.get("metadata").filter(|m| m.is_object()) else {
            bail!("document has no metadata");
        };
        let mut owners = Vec::new();
        for r in meta.get("ownerReferences").and_then(Value::as_array).into_iter().flatten() {
            owners.push(OwnerRef {
                api_version: s(r, "apiVersion").unwrap_or_default(),
                kind: s(r, "kind").unwrap_or_default(),
                name: s(r, "name").unwrap_or_default(),
                uid: s(r, "uid").unwrap_or_default(),
                controller: r.get("controller").and_then(Value::as_bool).unwrap_or(false),
            });
        }

        let entries = meta.get("managedFields").and_then(Value::as_array);
        let mut by_path: BTreeMap<String, Vec<Actor>> = BTreeMap::new();
        for e in entries.into_iter().flatten() {
            let Some(fields) = e.get("fieldsV1") else { continue };
            let time = s(e, "time");
            let actor = Actor {
                manager: s(e, "manager").unwrap_or_else(|| "<unknown>".into()),
                operation: s(e, "operation").unwrap_or_default(),
                epoch: time.as_deref().and_then(timeutil::parse_rfc3339),
                time,
                subresource: s(e, "subresource"),
            };
            for path in fieldsv1::flatten(fields) {
                by_path.entry(path).or_default().push(actor.clone());
            }
        }
        let fields = by_path
            .into_iter()
            .map(|(path, mut actors)| {
                // Newest first; undated entries last; manager name as stable tiebreak.
                actors.sort_by(|a, b| {
                    b.epoch.cmp(&a.epoch).then_with(|| a.manager.cmp(&b.manager))
                });
                Field { path, actors }
            })
            .collect();

        Ok(Object {
            api_version: s(v, "apiVersion").unwrap_or_default(),
            kind: s(v, "kind").unwrap_or_else(|| "<unknown>".into()),
            name: s(meta, "name").or_else(|| s(meta, "generateName")).unwrap_or_default(),
            namespace: s(meta, "namespace"),
            uid: s(meta, "uid"),
            owners,
            fields,
            entry_count: entries.map_or(0, Vec::len),
        })
    }

    /// The most recent write to any field of this object.
    pub fn latest(&self) -> Option<&Actor> {
        self.fields.iter().map(Field::last).filter(|a| a.epoch.is_some()).max_by_key(|a| a.epoch)
    }

    /// `Kind/name` with optional namespace, e.g. `Deployment/web (prod)`.
    pub fn label(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}/{} ({ns})", self.kind, self.name),
            None => format!("{}/{}", self.kind, self.name),
        }
    }
}

/// Field/actor filters shared by the CLI and the TUI.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// Substring (case-insensitive) or `*` glob matched against the field path.
    pub field: Option<String>,
    /// Substring (case-insensitive); keeps fields that *any* manager matching wrote.
    pub manager: Option<String>,
    /// Keep fields whose latest write is at or after this epoch.
    pub since_epoch: Option<i64>,
}

impl Filter {
    pub fn matches(&self, f: &Field) -> bool {
        if let Some(p) = &self.field
            && !path_matches(p, &f.path) {
                return false;
            }
        if let Some(m) = &self.manager {
            let m = m.to_lowercase();
            if !f.actors.iter().any(|a| a.manager.to_lowercase().contains(&m)) {
                return false;
            }
        }
        if let Some(cut) = self.since_epoch
            && !f.last().epoch.is_some_and(|e| e >= cut) {
                return false;
            }
        true
    }
}

pub fn path_matches(pattern: &str, path: &str) -> bool {
    let (pat, path) = (pattern.to_lowercase(), path.to_lowercase());
    if !pat.contains('*') {
        return path.contains(&pat);
    }
    // Anchored glob: `*` matches any run of characters.
    let parts: Vec<&str> = pat.split('*').collect();
    let mut rest = path.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(pos) = rest.find(part) else { return false };
            rest = &rest[pos + part.len()..];
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deploy() -> Value {
        json!({
          "apiVersion": "apps/v1", "kind": "Deployment",
          "metadata": {
            "name": "web", "namespace": "prod", "uid": "u1",
            "ownerReferences": [{"apiVersion":"x/v1","kind":"App","name":"a","uid":"u0","controller":true}],
            "managedFields": [
              {"manager":"kubectl","operation":"Update","time":"2024-01-01T00:00:00Z",
               "fieldsV1":{"f:spec":{"f:replicas":{},"f:paused":{}}}},
              {"manager":"hpa","operation":"Update","time":"2024-02-01T00:00:00Z","subresource":"scale",
               "fieldsV1":{"f:spec":{"f:replicas":{}}}},
              {"manager":"old","operation":"Update",
               "fieldsV1":{"f:spec":{"f:replicas":{}}}}
            ]}})
    }

    #[test]
    fn newest_writer_wins_and_all_actors_kept() {
        let o = Object::from_value(&deploy()).unwrap();
        assert_eq!(o.entry_count, 3);
        let r = o.fields.iter().find(|f| f.path == "spec.replicas").unwrap();
        let order: Vec<_> = r.actors.iter().map(|a| a.manager.as_str()).collect();
        assert_eq!(order, ["hpa", "kubectl", "old"]); // undated last
        assert_eq!(r.last().subresource.as_deref(), Some("scale"));
        let p = o.fields.iter().find(|f| f.path == "spec.paused").unwrap();
        assert_eq!(p.last().manager, "kubectl");
        assert_eq!(o.owners[0].uid, "u0");
        assert_eq!(o.label(), "Deployment/web (prod)");
    }

    #[test]
    fn latest_write_across_fields() {
        let o = Object::from_value(&deploy()).unwrap();
        let a = o.latest().unwrap();
        assert_eq!((a.manager.as_str(), a.time.as_deref()), ("hpa", Some("2024-02-01T00:00:00Z")));
        let empty = Object::from_value(&json!({"kind": "Pod", "metadata": {"name": "x"}})).unwrap();
        assert!(empty.latest().is_none());
    }

    #[test]
    fn filters() {
        let o = Object::from_value(&deploy()).unwrap();
        let count = |f: Filter| o.fields.iter().filter(|x| f.matches(x)).count();
        assert_eq!(count(Filter::default()), 2);
        assert_eq!(count(Filter { manager: Some("HPA".into()), ..Default::default() }), 1);
        assert_eq!(count(Filter { field: Some("spec.*ed".into()), ..Default::default() }), 1);
        let cut = timeutil::parse_rfc3339("2024-01-15T00:00:00Z");
        assert_eq!(count(Filter { since_epoch: cut, ..Default::default() }), 1);
    }

    #[test]
    fn globs() {
        assert!(path_matches("spec.*", "spec.replicas"));
        assert!(!path_matches("spec.*", "metadata.spec.x"));
        assert!(path_matches("*image", "spec.containers[name=a].image"));
        assert!(path_matches("a*c*e", "abcde"));
        assert!(path_matches("labels", "metadata.labels.app"));
    }

    #[test]
    fn rejects_non_objects() {
        assert!(Object::from_value(&json!({"kind": "X"})).is_err());
    }
}
