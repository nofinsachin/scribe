//! Owner-chain resolution across the loaded set of objects.

use crate::model::{Object, OwnerRef};

#[derive(Debug, Clone)]
pub struct Link {
    pub reference: OwnerRef,
    /// Index into the object set if the owner was part of the input.
    pub resolved: Option<usize>,
}

fn find(objs: &[Object], r: &OwnerRef) -> Option<usize> {
    objs.iter()
        .position(|o| !r.uid.is_empty() && o.uid.as_deref() == Some(r.uid.as_str()))
        .or_else(|| objs.iter().position(|o| o.kind == r.kind && o.name == r.name))
}

/// Walk upward from `idx` following the controller reference (or the first
/// reference). Stops at an unresolved owner, a root, or a cycle.
pub fn chain(objs: &[Object], idx: usize) -> Vec<Link> {
    let mut out = Vec::new();
    let mut seen = vec![idx];
    let mut cur = idx;
    while let Some(r) = objs[cur].owners.iter().find(|r| r.controller).or(objs[cur].owners.first()) {
        let resolved = find(objs, r);
        out.push(Link { reference: r.clone(), resolved });
        match resolved {
            Some(next) if !seen.contains(&next) => {
                seen.push(next);
                cur = next;
            }
            _ => break,
        }
    }
    out
}

/// Objects in the set that list `idx` as an owner.
pub fn children(objs: &[Object], idx: usize) -> Vec<usize> {
    (0..objs.len())
        .filter(|&i| i != idx && objs[i].owners.iter().any(|r| find(objs, r) == Some(idx)))
        .collect()
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "Deployment" => 0,
        "StatefulSet" => 1,
        "DaemonSet" => 2,
        "CronJob" => 3,
        "Job" => 4,
        "ReplicaSet" => 5,
        "Pod" => 6,
        "Service" => 7,
        _ => 8,
    }
}

/// Opinionated display order for an object picker: owner trees, roots first
/// (controllers before pods before the rest), children nested under their owner.
/// Returns `(index, depth)`; every object appears exactly once, even in cycles.
pub fn tree_order(objs: &[Object]) -> Vec<(usize, usize)> {
    let n = objs.len();
    let parent: Vec<Option<usize>> = (0..n)
        .map(|i| {
            let r = objs[i].owners.iter().find(|r| r.controller).or(objs[i].owners.first())?;
            find(objs, r).filter(|&p| p != i)
        })
        .collect();
    let key = |i: &usize| (kind_rank(&objs[*i].kind), objs[*i].name.clone());
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, p) in parent.iter().enumerate() {
        match p {
            Some(p) => kids[*p].push(i),
            None => roots.push(i),
        }
    }
    roots.sort_by_key(key);
    kids.iter_mut().for_each(|k| k.sort_by_key(key));

    fn visit(i: usize, depth: usize, kids: &[Vec<usize>], seen: &mut [bool], out: &mut Vec<(usize, usize)>) {
        if std::mem::replace(&mut seen[i], true) {
            return;
        }
        out.push((i, depth));
        for &k in &kids[i] {
            visit(k, depth + 1, kids, seen, out);
        }
    }
    let (mut seen, mut out) = (vec![false; n], Vec::with_capacity(n));
    for r in roots {
        visit(r, 0, &kids, &mut seen, &mut out);
    }
    for i in 0..n {
        visit(i, 0, &kids, &mut seen, &mut out); // owner cycles: nothing is lost
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::parse_documents;

    const SET: &str = "\
kind: Deployment
metadata: {name: web, uid: d1}
---
kind: ReplicaSet
metadata:
  name: web-abc
  uid: r1
  ownerReferences: [{apiVersion: apps/v1, kind: Deployment, name: web, uid: d1, controller: true}]
---
kind: Pod
metadata:
  name: web-abc-x
  uid: p1
  ownerReferences: [{apiVersion: apps/v1, kind: ReplicaSet, name: web-abc, uid: r1, controller: true}]
---
kind: Pod
metadata:
  name: orphan
  ownerReferences: [{apiVersion: v1, kind: Node, name: n1, uid: n}]
";

    #[test]
    fn walks_chain_to_root() {
        let o = parse_documents(SET).unwrap();
        let c = chain(&o, 2);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].resolved, Some(1));
        assert_eq!(c[1].resolved, Some(0));
    }

    #[test]
    fn unresolved_owner_is_reported() {
        let o = parse_documents(SET).unwrap();
        let c = chain(&o, 3);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].resolved, None);
        assert_eq!(c[0].reference.kind, "Node");
    }

    #[test]
    fn tree_order_nests_children_under_owners() {
        let o = parse_documents(SET).unwrap();
        // Deployment(0) > ReplicaSet(1) > Pod(2); orphan Pod(3) is its own root, after the Deployment.
        assert_eq!(tree_order(&o), vec![(0, 0), (1, 1), (2, 2), (3, 0)]);
    }

    #[test]
    fn tree_order_covers_cycles_and_self_owners() {
        let cyc = parse_documents(
            "kind: A\nmetadata: {name: a, uid: a, ownerReferences: [{kind: B, name: b, uid: b}]}\n---\nkind: B\nmetadata: {name: b, uid: b, ownerReferences: [{kind: A, name: a, uid: a}]}\n---\nkind: C\nmetadata: {name: c, uid: c, ownerReferences: [{kind: C, name: c, uid: c}]}\n",
        )
        .unwrap();
        let mut idx: Vec<usize> = tree_order(&cyc).iter().map(|x| x.0).collect();
        idx.sort();
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn children_and_cycles() {
        let o = parse_documents(SET).unwrap();
        assert_eq!(children(&o, 0), vec![1]);
        let cyc = parse_documents(
            "kind: A\nmetadata: {name: a, uid: a, ownerReferences: [{kind: B, name: b, uid: b}]}\n---\nkind: B\nmetadata: {name: b, uid: b, ownerReferences: [{kind: A, name: a, uid: a}]}\n",
        )
        .unwrap();
        assert_eq!(chain(&cyc, 0).len(), 2); // terminates
    }
}
