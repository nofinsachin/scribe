//! Loading objects from files/stdin (YAML or JSON, multi-document, `List` kinds).

use crate::model::Object;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;

/// Parse every document in `text`. `List`-style kinds (`items: [...]`) are expanded.
pub fn parse_documents(text: &str) -> Result<Vec<Object>> {
    let mut out = Vec::new();
    for (n, doc) in serde_yaml::Deserializer::from_str(text).enumerate() {
        let v = Value::deserialize(doc).with_context(|| format!("parsing document #{}", n + 1))?;
        collect(&v, &mut out)?;
    }
    Ok(out)
}

fn collect(v: &Value, out: &mut Vec<Object>) -> Result<()> {
    match v {
        Value::Null => Ok(()),
        // `kubectl get a,b -o yaml` yields a List that has its own (empty) metadata.
        Value::Object(m) if m.get("items").is_some_and(Value::is_array) && m.get("kind").and_then(Value::as_str).is_none_or(|k| k.ends_with("List")) => {
            for item in m["items"].as_array().into_iter().flatten() {
                collect(item, out)?;
            }
            Ok(())
        }
        Value::Object(_) => {
            out.push(Object::from_value(v)?);
            Ok(())
        }
        _ => bail!("expected a Kubernetes object, found a scalar or list"),
    }
}

/// Read objects from the given paths; `-` or an empty list means stdin.
pub fn load(paths: &[String]) -> Result<Vec<Object>> {
    let mut all = Vec::new();
    if paths.is_empty() {
        all.extend(read_one("-")?);
    }
    for p in paths {
        all.extend(read_one(p)?);
    }
    if all.is_empty() {
        bail!("no objects found in input");
    }
    Ok(all)
}

fn read_one(path: &str) -> Result<Vec<Object>> {
    let mut text = String::new();
    if path == "-" {
        std::io::stdin().read_to_string(&mut text).context("reading stdin")?;
    } else {
        text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    }
    parse_documents(&text).with_context(|| format!("in {}", if path == "-" { "<stdin>" } else { path }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_doc_yaml_and_lists() {
        let y = "\
kind: Pod
metadata: {name: a}
---
kind: List
items:
  - kind: Pod
    metadata: {name: b}
  - kind: Pod
    metadata: {name: c}
---
";
        let objs = parse_documents(y).unwrap();
        let names: Vec<_> = objs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn kubectl_list_with_own_metadata() {
        let y = "kind: List\nmetadata: {}\nitems:\n- {kind: Pod, metadata: {name: a}}\n";
        let objs = parse_documents(y).unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].name, "a");
    }

    #[test]
    fn json_input() {
        let objs = parse_documents(r#"{"kind":"Pod","metadata":{"name":"j"}}"#).unwrap();
        assert_eq!(objs[0].name, "j");
        assert!(objs[0].fields.is_empty());
    }

    #[test]
    fn bad_input_errors() {
        assert!(parse_documents("just a string").is_err());
        assert!(parse_documents("kind: Pod").is_err()); // no metadata
    }
}
