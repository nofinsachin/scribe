//! Non-interactive renderers: table, TSV, JSON.

use crate::model::{Filter, Object};
use crate::owners;
use crate::timeutil::human_age;
use serde_json::{Value, json};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Table,
    Tsv,
    Json,
}

fn age(epoch: Option<i64>, now: i64) -> String {
    epoch.map_or("-".into(), |e| human_age(now - e))
}

fn actor_name(manager: &str, sub: &Option<String>) -> String {
    match sub {
        Some(s) if !s.is_empty() => format!("{manager} ({s})"),
        _ => manager.to_string(),
    }
}

/// Render all objects. `now` is injected for deterministic tests.
pub fn render(objs: &[Object], filter: &Filter, format: Format, now: i64) -> String {
    match format {
        Format::Json => render_json(objs, filter, now),
        Format::Tsv => render_rows(objs, filter, now, false),
        Format::Table => render_rows(objs, filter, now, true),
    }
}

fn render_rows(objs: &[Object], filter: &Filter, now: i64, pretty: bool) -> String {
    let mut out = String::new();
    for (i, o) in objs.iter().enumerate() {
        let rows: Vec<[String; 6]> = o
            .fields
            .iter()
            .filter(|f| filter.matches(f))
            .map(|f| {
                let a = f.last();
                [
                    f.path.clone(),
                    actor_name(&a.manager, &a.subresource),
                    a.operation.clone(),
                    a.time.clone().unwrap_or_else(|| "-".into()),
                    age(a.epoch, now),
                    if f.actors.len() > 1 { format!("+{}", f.actors.len() - 1) } else { String::new() },
                ]
            })
            .collect();
        if pretty {
            if i > 0 {
                out.push('\n');
            }
            let _ = writeln!(out, "# {}", o.label());
            let chain = owners::chain(objs, i);
            if !chain.is_empty() {
                let names: Vec<String> = chain
                    .iter()
                    .map(|l| format!("{}/{}{}", l.reference.kind, l.reference.name, if l.resolved.is_some() { "" } else { "?" }))
                    .collect();
                let _ = writeln!(out, "# owned by: {}", names.join(" <- "));
            }
            let hdr = ["FIELD", "LAST WRITER", "OP", "TIME", "AGE", "ALSO"].map(String::from);
            let mut w: [usize; 6] = hdr.clone().map(|h| h.len());
            for r in &rows {
                for (c, cell) in r.iter().enumerate() {
                    w[c] = w[c].max(cell.chars().count());
                }
            }
            for r in std::iter::once(&hdr).chain(rows.iter()) {
                let line: Vec<String> = r.iter().enumerate().map(|(c, cell)| format!("{cell:<w$}", w = w[c])).collect();
                let _ = writeln!(out, "{}", line.join("  ").trim_end());
            }
        } else {
            for r in &rows {
                let _ = writeln!(out, "{}\t{}\t{}", o.label(), r[0], r[1..5].join("\t"));
            }
        }
    }
    out
}

fn render_json(objs: &[Object], filter: &Filter, now: i64) -> String {
    let items: Vec<Value> = objs
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let fields: Vec<Value> = o
                .fields
                .iter()
                .filter(|f| filter.matches(f))
                .map(|f| {
                    let actors: Vec<Value> = f
                        .actors
                        .iter()
                        .map(|a| {
                            json!({
                                "manager": a.manager, "operation": a.operation, "time": a.time,
                                "ageSeconds": a.epoch.map(|e| now - e), "subresource": a.subresource,
                            })
                        })
                        .collect();
                    json!({"path": f.path, "lastWriter": actors[0], "actors": actors})
                })
                .collect();
            let chain: Vec<Value> = owners::chain(objs, i)
                .iter()
                .map(|l| json!({"kind": l.reference.kind, "name": l.reference.name, "uid": l.reference.uid,
                                "controller": l.reference.controller, "resolved": l.resolved.is_some()}))
                .collect();
            json!({
                "apiVersion": o.api_version, "kind": o.kind, "name": o.name,
                "namespace": o.namespace, "uid": o.uid, "ownerChain": chain, "fields": fields,
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).expect("json serialization") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{input::parse_documents, timeutil::parse_rfc3339};

    const Y: &str = "\
kind: Deployment
metadata:
  name: web
  uid: d1
  managedFields:
    - {manager: kubectl, operation: Update, time: '2024-01-01T00:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}
    - {manager: hpa, operation: Update, subresource: scale, time: '2024-01-01T01:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}
---
kind: ReplicaSet
metadata:
  name: rs
  ownerReferences: [{kind: Deployment, name: web, uid: d1, controller: true}]
  managedFields:
    - {manager: kube-controller-manager, operation: Update, time: '2024-01-01T00:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}
";

    fn now() -> i64 {
        parse_rfc3339("2024-01-01T02:00:00Z").unwrap()
    }

    #[test]
    fn table_shows_last_writer_age_and_chain() {
        let o = parse_documents(Y).unwrap();
        let t = render(&o, &Filter::default(), Format::Table, now());
        assert!(t.contains("# Deployment/web"), "{t}");
        assert!(t.contains("hpa (scale)"), "{t}");
        assert!(t.contains("1h0m") && t.contains("+1"), "{t}");
        assert!(t.contains("# owned by: Deployment/web"), "{t}");
    }

    #[test]
    fn tsv_is_machine_friendly() {
        let o = parse_documents(Y).unwrap();
        let t = render(&o, &Filter::default(), Format::Tsv, now());
        let first = t.lines().next().unwrap();
        assert_eq!(first.split('\t').count(), 6);
        assert!(first.starts_with("Deployment/web\tspec.replicas\thpa (scale)\tUpdate"));
    }

    #[test]
    fn json_roundtrips() {
        let o = parse_documents(Y).unwrap();
        let t = render(&o, &Filter::default(), Format::Json, now());
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v[0]["fields"][0]["lastWriter"]["manager"], "hpa");
        assert_eq!(v[0]["fields"][0]["actors"].as_array().unwrap().len(), 2);
        assert_eq!(v[1]["ownerChain"][0]["resolved"], true);
    }

    #[test]
    fn filter_applies() {
        let o = parse_documents(Y).unwrap();
        let f = Filter { manager: Some("controller".into()), ..Default::default() };
        let t = render(&o, &f, Format::Tsv, now());
        assert_eq!(t.lines().count(), 1);
        assert!(t.starts_with("ReplicaSet/rs"));
    }
}
