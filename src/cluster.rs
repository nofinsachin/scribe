//! Cluster discovery (kubeconfig / in-cluster) and a thin kubectl-backed fetcher.
//!
//! Discovery reads only non-secret structure (context, cluster, server, user *name*,
//! namespace). Credentials are never parsed; authentication is delegated to `kubectl`.

use serde::Deserialize;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Kubeconfig(Vec<PathBuf>),
    InCluster,
}

/// Everything we know about where we'd connect, before connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub source: Source,
    pub context: Option<String>,
    pub cluster: String,
    pub server: String,
    pub user: Option<String>,
    /// Default namespace of the context (or the service account's).
    pub namespace: Option<String>,
    pub contexts_total: usize,
    /// Passed to kubectl only when the user gave `--kubeconfig` explicitly.
    pub kubeconfig_flag: Option<PathBuf>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub target: Option<Target>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub kubeconfig: Option<PathBuf>,
    pub context: Option<String>,
}

/// Process environment, injectable for tests.
#[derive(Debug, Clone)]
pub struct Env {
    pub kubeconfig_env: Option<OsString>,
    pub home: Option<PathBuf>,
    pub service_host: Option<String>,
    pub service_port: Option<String>,
    pub sa_dir: PathBuf,
}

impl Env {
    pub fn from_process() -> Env {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        Env {
            kubeconfig_env: var("KUBECONFIG"),
            home: var("HOME").map(PathBuf::from),
            service_host: var("KUBERNETES_SERVICE_HOST").and_then(|v| v.into_string().ok()),
            service_port: var("KUBERNETES_SERVICE_PORT").and_then(|v| v.into_string().ok()),
            sa_dir: PathBuf::from("/var/run/secrets/kubernetes.io/serviceaccount"),
        }
    }
}

// ---- kubeconfig parsing (structure only; unknown fields such as credentials are ignored)

#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(rename = "current-context", default)]
    current_context: Option<String>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
    #[serde(default)]
    clusters: Vec<NamedCluster>,
}
#[derive(Deserialize)]
struct NamedContext {
    name: String,
    #[serde(default)]
    context: RawContext,
}
#[derive(Deserialize, Default)]
struct RawContext {
    #[serde(default)]
    cluster: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}
#[derive(Deserialize)]
struct NamedCluster {
    name: String,
    #[serde(default)]
    cluster: RawCluster,
}
#[derive(Deserialize, Default)]
struct RawCluster {
    #[serde(default)]
    server: String,
}

pub fn discover(env: &Env, ov: &Overrides) -> Discovery {
    let mut d = Discovery::default();
    let mut looked = Vec::new();

    let candidates: Vec<PathBuf> = if let Some(p) = &ov.kubeconfig {
        looked.push(format!("--kubeconfig {}", p.display()));
        vec![p.clone()]
    } else if let Some(k) = &env.kubeconfig_env {
        looked.push("$KUBECONFIG".into());
        std::env::split_paths(k).filter(|p| !p.as_os_str().is_empty()).collect()
    } else if let Some(h) = &env.home {
        looked.push("~/.kube/config".into());
        vec![h.join(".kube").join("config")]
    } else {
        vec![]
    };

    let mut loaded: Vec<(PathBuf, RawConfig)> = Vec::new();
    for p in &candidates {
        match std::fs::read_to_string(p) {
            Ok(text) => match serde_yaml::from_str::<RawConfig>(&text) {
                Ok(c) => loaded.push((p.clone(), c)),
                Err(e) => d.warnings.push(format!("can't parse kubeconfig {}: {e}", p.display())),
            },
            // A missing default ~/.kube/config is normal; only complain about explicit paths.
            Err(e) if ov.kubeconfig.is_some() || env.kubeconfig_env.is_some() => {
                d.warnings.push(format!("can't read kubeconfig {}: {e}", p.display()))
            }
            Err(_) => {}
        }
    }

    if !loaded.is_empty() {
        match build_target(&loaded, ov) {
            Ok(mut t) => {
                t.kubeconfig_flag = ov.kubeconfig.clone();
                d.target = Some(t);
                return d;
            }
            Err(w) => d.warnings.push(w),
        }
    }

    looked.push("in-cluster service account".into());
    if let (Some(host), Some(port)) = (&env.service_host, &env.service_port)
        && env.sa_dir.join("token").exists() {
            let host = if host.contains(':') { format!("[{host}]") } else { host.clone() };
            let ns = std::fs::read_to_string(env.sa_dir.join("namespace")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            d.target = Some(Target {
                source: Source::InCluster,
                context: None,
                cluster: "in-cluster".into(),
                server: format!("https://{host}:{port}"),
                user: Some("serviceaccount".into()),
                namespace: ns,
                contexts_total: 0,
                kubeconfig_flag: None,
            });
            return d;
        }

    if d.warnings.is_empty() {
        d.warnings.push(format!("couldn't discover any k8s cluster (looked at: {})", looked.join(", ")));
    }
    d
}

fn build_target(loaded: &[(PathBuf, RawConfig)], ov: &Overrides) -> Result<Target, String> {
    // Like kubectl: first file wins for current-context and for duplicate names.
    let mut contexts: Vec<&NamedContext> = Vec::new();
    let mut clusters: Vec<&NamedCluster> = Vec::new();
    let (mut seen_ctx, mut seen_cl) = (HashSet::new(), HashSet::new());
    for (_, c) in loaded {
        contexts.extend(c.contexts.iter().filter(|x| seen_ctx.insert(x.name.as_str())));
        clusters.extend(c.clusters.iter().filter(|x| seen_cl.insert(x.name.as_str())));
    }
    let paths: Vec<PathBuf> = loaded.iter().map(|(p, _)| p.clone()).collect();
    let shown = |ps: &[PathBuf]| ps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":");

    if contexts.is_empty() {
        return Err(format!("kubeconfig {} has no contexts", shown(&paths)));
    }
    let wanted = ov
        .context
        .clone()
        .or_else(|| loaded.iter().find_map(|(_, c)| c.current_context.clone().filter(|s| !s.is_empty())));
    let ctx = match &wanted {
        Some(w) => contexts.iter().find(|c| &c.name == w).copied().ok_or_else(|| {
            format!("context {w:?} not found in {} (available: {})", shown(&paths), contexts.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "))
        })?,
        None => contexts[0],
    };
    let server = clusters.iter().find(|c| c.name == ctx.context.cluster).map(|c| c.cluster.server.clone()).unwrap_or_default();
    Ok(Target {
        source: Source::Kubeconfig(paths),
        context: Some(ctx.name.clone()),
        cluster: ctx.context.cluster.clone(),
        server: if server.is_empty() { "<unknown server>".into() } else { server },
        user: ctx.context.user.clone(),
        namespace: ctx.context.namespace.clone().filter(|s| !s.is_empty()),
        contexts_total: contexts.len(),
        kubeconfig_flag: None,
    })
}

// ---- namespaces, ordering ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace {
    pub name: String,
    pub phase: String,
    pub created: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsKind {
    /// The namespace of the active context.
    Current,
    User,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NsRow {
    pub ns: Namespace,
    pub kind: NsKind,
}

/// Opinionated default order: the context's namespace first, then everything
/// else alphabetically, with `kube-*` system namespaces sunk to the bottom.
pub fn order_namespaces(mut all: Vec<Namespace>, current: Option<&str>) -> Vec<NsRow> {
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all.dedup_by(|a, b| a.name == b.name);
    let (mut cur, mut user, mut sys) = (Vec::new(), Vec::new(), Vec::new());
    for ns in all {
        if Some(ns.name.as_str()) == current {
            cur.push(NsRow { ns, kind: NsKind::Current });
        } else if ns.name.starts_with("kube-") {
            sys.push(NsRow { ns, kind: NsKind::System });
        } else {
            user.push(NsRow { ns, kind: NsKind::User });
        }
    }
    cur.into_iter().chain(user).chain(sys).collect()
}

// ---- fetching (kubectl subprocess; swap this module section for a native client later) ----

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fetched {
    pub version: Option<String>,
    pub namespaces: Vec<Namespace>,
    /// Non-fatal caveat to surface in the UI, e.g. RBAC forbids listing namespaces.
    pub note: Option<String>,
}

pub fn find_in_path(bin: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|d| d.join(bin)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Run kubectl; returns (stdout, success, cleaned stderr).
fn kubectl_raw(bin: &Path, t: &Target, args: &[&str]) -> Result<(String, bool, String), String> {
    let mut cmd = Command::new(bin);
    if let Some(k) = &t.kubeconfig_flag {
        cmd.arg("--kubeconfig").arg(k);
    }
    if let Some(c) = &t.context {
        cmd.arg("--context").arg(c);
    }
    cmd.arg("--request-timeout=8s").args(args).stdin(Stdio::null());
    let out = cmd.output().map_err(|e| format!("running kubectl: {e}"))?;
    let err = String::from_utf8_lossy(&out.stderr);
    let err = if err.trim().is_empty() { String::new() } else { clean_kubectl_error(&err) };
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success(), err))
}

fn kubectl(bin: &Path, t: &Target, args: &[&str]) -> Result<String, String> {
    match kubectl_raw(bin, t, args)? {
        (out, true, _) => Ok(out),
        (_, false, err) => Err(if err.is_empty() { "kubectl failed".into() } else { err }),
    }
}

/// Kinds loaded for a namespace. Deliberately no Secrets (their data would be
/// pulled along) and nothing cluster-scoped.
pub const NAMESPACE_KINDS: &str = "deployments,statefulsets,daemonsets,replicasets,jobs,cronjobs,pods,services";

/// Objects of a namespace with their managedFields, plus an optional caveat
/// (e.g. one kind was forbidden) when the result is partial.
pub fn fetch_objects(bin: &Path, t: &Target, namespace: &str) -> Result<(Vec<crate::model::Object>, Option<String>), String> {
    let (out, ok, err) = kubectl_raw(bin, t, &["get", NAMESPACE_KINDS, "-n", namespace, "-o", "json", "--show-managed-fields", "--request-timeout=30s"])?;
    if out.trim().is_empty() {
        return Err(if err.is_empty() { "kubectl returned nothing".into() } else { err });
    }
    let objs = crate::input::parse_documents(&out).map_err(|e| format!("{e:#}"))?;
    let note = (!ok && !err.is_empty()).then(|| format!("partial result: {err}"));
    Ok((objs, note))
}

/// kubectl interleaves klog noise (`E1113 18:06:55.7 memcache.go:265] ...`) with the
/// human message, which comes last. Prefer that; fall back to the first klog line.
pub fn clean_kubectl_error(stderr: &str) -> String {
    let is_klog = |l: &str| {
        let b = l.as_bytes();
        b.len() > 5 && matches!(b[0], b'E' | b'W' | b'I' | b'F') && b[1..5].iter().all(u8::is_ascii_digit)
    };
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let line = lines.iter().rev().find(|l| !is_klog(l)).or(lines.first()).copied().unwrap_or("kubectl failed");
    line.chars().take(200).collect()
}

pub fn fetch(bin: &Path, t: &Target) -> Result<Fetched, String> {
    let mut f = Fetched::default();
    match kubectl(bin, t, &["get", "namespaces", "-o", "json"]) {
        Ok(json) => f.namespaces = parse_namespaces(&json)?,
        Err(e) if e.to_lowercase().contains("forbidden") => {
            f.note = Some("not allowed to list namespaces; showing the context namespace only".into());
        }
        Err(e) => return Err(e),
    }
    // Best effort: the version is a nicety, not a connectivity requirement.
    f.version = kubectl(bin, t, &["version", "-o", "json"]).ok().and_then(|j| parse_server_version(&j));
    Ok(f)
}

pub fn parse_namespaces(json: &str) -> Result<Vec<Namespace>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("bad namespaces response: {e}"))?;
    let items = v.get("items").and_then(|i| i.as_array()).ok_or("bad namespaces response: no items")?;
    Ok(items
        .iter()
        .filter_map(|i| {
            let name = i.pointer("/metadata/name")?.as_str()?.to_string();
            Some(Namespace {
                name,
                phase: i.pointer("/status/phase").and_then(|p| p.as_str()).unwrap_or("Unknown").to_string(),
                created: i.pointer("/metadata/creationTimestamp").and_then(|t| t.as_str()).and_then(crate::timeutil::parse_rfc3339),
            })
        })
        .collect())
}

pub fn parse_server_version(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(v.pointer("/serverVersion/gitVersion")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!("scribe-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const KC: &str = "\
apiVersion: v1
kind: Config
current-context: dev
contexts:
- name: dev
  context: {cluster: c1, user: u1, namespace: team-a}
- name: prod
  context: {cluster: c2, user: u2}
clusters:
- name: c1
  cluster: {server: 'https://1.2.3.4:6443', certificate-authority-data: AAAA}
- name: c2
  cluster: {server: 'https://5.6.7.8'}
users:
- name: u1
  user: {token: SECRET-TOKEN}
";

    fn env(t: &Tmp) -> Env {
        Env { kubeconfig_env: None, home: Some(t.0.clone()), service_host: None, service_port: None, sa_dir: t.0.join("sa") }
    }

    #[test]
    fn finds_default_kubeconfig() {
        let t = Tmp::new();
        t.write(".kube/config", KC);
        let d = discover(&env(&t), &Overrides::default());
        let tg = d.target.unwrap();
        assert!(d.warnings.is_empty());
        assert_eq!(tg.context.as_deref(), Some("dev"));
        assert_eq!(tg.server, "https://1.2.3.4:6443");
        assert_eq!(tg.namespace.as_deref(), Some("team-a"));
        assert_eq!(tg.user.as_deref(), Some("u1"));
        assert_eq!(tg.contexts_total, 2);
        assert_eq!(tg.kubeconfig_flag, None);
        assert!(!format!("{tg:?}").contains("SECRET"));
    }

    #[test]
    fn context_override_and_unknown_context() {
        let t = Tmp::new();
        t.write(".kube/config", KC);
        let ov = Overrides { context: Some("prod".into()), ..Default::default() };
        assert_eq!(discover(&env(&t), &ov).target.unwrap().server, "https://5.6.7.8");
        let bad = Overrides { context: Some("nope".into()), ..Default::default() };
        let d = discover(&env(&t), &bad);
        assert!(d.target.is_none());
        assert!(d.warnings[0].contains("\"nope\" not found") && d.warnings[0].contains("dev, prod"), "{:?}", d.warnings);
    }

    #[test]
    fn kubeconfig_env_list_merges_first_wins() {
        let t = Tmp::new();
        let a = t.write("a.yaml", "current-context: x\ncontexts: [{name: x, context: {cluster: ca}}]\nclusters: [{name: ca, cluster: {server: 'https://a'}}]\n");
        let b = t.write("b.yaml", "current-context: y\ncontexts: [{name: y, context: {cluster: cb}}, {name: x, context: {cluster: WRONG}}]\nclusters: [{name: cb, cluster: {server: 'https://b'}}]\n");
        let mut e = env(&t);
        e.kubeconfig_env = Some(std::env::join_paths([&a, &b]).unwrap());
        let d = discover(&e, &Overrides::default());
        let tg = d.target.unwrap();
        assert_eq!((tg.context.as_deref(), tg.server.as_str(), tg.contexts_total), (Some("x"), "https://a", 2));
        let ov = Overrides { context: Some("y".into()), ..Default::default() };
        assert_eq!(discover(&e, &ov).target.unwrap().server, "https://b");
    }

    #[test]
    fn explicit_flag_is_remembered_for_kubectl() {
        let t = Tmp::new();
        let p = t.write("custom", KC);
        let ov = Overrides { kubeconfig: Some(p.clone()), ..Default::default() };
        assert_eq!(discover(&env(&t), &ov).target.unwrap().kubeconfig_flag, Some(p));
    }

    #[test]
    fn nothing_found_warns() {
        let t = Tmp::new();
        let d = discover(&env(&t), &Overrides::default());
        assert!(d.target.is_none());
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].starts_with("couldn't discover any k8s cluster"), "{:?}", d.warnings);
    }

    #[test]
    fn broken_kubeconfig_warns_and_falls_through() {
        let t = Tmp::new();
        t.write(".kube/config", "contexts: [[[");
        let d = discover(&env(&t), &Overrides::default());
        assert!(d.target.is_none());
        assert_eq!(d.warnings.len(), 1, "specific warning only: {:?}", d.warnings);
        assert!(d.warnings[0].contains("can't parse kubeconfig"));
    }

    #[test]
    fn missing_explicit_kubeconfig_warns() {
        let t = Tmp::new();
        let ov = Overrides { kubeconfig: Some(t.0.join("absent")), ..Default::default() };
        let d = discover(&env(&t), &ov);
        assert!(d.warnings.iter().any(|w| w.contains("can't read kubeconfig")), "{:?}", d.warnings);
    }

    #[test]
    fn in_cluster_fallback() {
        let t = Tmp::new();
        t.write("sa/token", "x");
        t.write("sa/namespace", "payments\n");
        let mut e = env(&t);
        e.service_host = Some("10.0.0.1".into());
        e.service_port = Some("443".into());
        let tg = discover(&e, &Overrides::default()).target.unwrap();
        assert_eq!(tg.source, Source::InCluster);
        assert_eq!(tg.server, "https://10.0.0.1:443");
        assert_eq!(tg.namespace.as_deref(), Some("payments"));
        e.service_host = Some("fd00::1".into());
        assert_eq!(discover(&e, &Overrides::default()).target.unwrap().server, "https://[fd00::1]:443");
    }

    fn ns(n: &str) -> Namespace {
        Namespace { name: n.into(), phase: "Active".into(), created: None }
    }

    #[test]
    fn kubectl_errors_are_humanized() {
        let noisy = "E1113 18:06:55.730138   47758 memcache.go:265] \"Unhandled Error\" err=\"x\"\nE1113 18:06:55.7 a.go:1] \"y\"\nThe connection to the server 127.0.0.1:1 was refused\n";
        assert_eq!(clean_kubectl_error(noisy), "The connection to the server 127.0.0.1:1 was refused");
        assert_eq!(clean_kubectl_error("Unable to connect to the server: x509"), "Unable to connect to the server: x509");
        assert!(clean_kubectl_error("E1113 18:06:55.7 a.go:1] only klog").starts_with("E1113"));
        assert_eq!(clean_kubectl_error(""), "kubectl failed");
        assert_eq!(clean_kubectl_error(&"x".repeat(500)).len(), 200);
    }

    #[test]
    fn namespace_ordering_is_opinionated() {
        let rows = order_namespaces(vec![ns("kube-system"), ns("zeta"), ns("default"), ns("alpha"), ns("kube-public"), ns("team-a")], Some("team-a"));
        let names: Vec<_> = rows.iter().map(|r| r.ns.name.as_str()).collect();
        assert_eq!(names, ["team-a", "alpha", "default", "zeta", "kube-public", "kube-system"]);
        assert_eq!(rows[0].kind, NsKind::Current);
        assert_eq!(rows[5].kind, NsKind::System);
        // current namespace absent from the list: nothing is marked current
        assert!(order_namespaces(vec![ns("a")], Some("gone")).iter().all(|r| r.kind != NsKind::Current));
    }

    #[test]
    fn parses_kubectl_json() {
        let j = r#"{"items":[{"metadata":{"name":"default","creationTimestamp":"2024-01-01T00:00:00Z"},"status":{"phase":"Active"}},{"metadata":{"name":"old"},"status":{"phase":"Terminating"}}]}"#;
        let v = parse_namespaces(j).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].created, Some(1_704_067_200));
        assert_eq!(v[1].phase, "Terminating");
        assert!(parse_namespaces("{}").is_err());
        assert!(parse_namespaces("nope").is_err());
        assert_eq!(parse_server_version(r#"{"serverVersion":{"gitVersion":"v1.31.0"}}"#).as_deref(), Some("v1.31.0"));
        assert_eq!(parse_server_version(r#"{"clientVersion":{}}"#), None);
    }
}
