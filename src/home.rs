//! Home screen shown when scribe is started without input: cluster details,
//! warnings, and an opinionated namespace list. Loads instantly; the cluster is
//! queried on a background thread.

use crate::cluster::{self, Discovery, Fetched, NsKind, NsRow, Source, Target};
use crate::model::{Filter, Object};
use crate::{timeutil, tui};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    /// Nothing to connect to.
    NoCluster,
    NoKubectl,
    Connecting,
    Connected { version: Option<String> },
    Failed(String),
}

pub struct Home {
    pub discovery: Discovery,
    pub kubectl: Option<PathBuf>,
    pub conn: Conn,
    pub rows: Vec<NsRow>,
    pub note: Option<String>,
    pub list: ListState,
    pub now: i64,
    rx: Option<Receiver<Result<Fetched, String>>>,
    /// Namespace being loaded into the explorer, with its result channel.
    opening: Option<(String, Receiver<OpenResult>)>,
    /// Why the last attempt to open a namespace failed / came back partial.
    pub open_note: Option<String>,
    /// The authorship explorer, once a namespace is open.
    pub browse: Option<tui::App>,
}

pub type OpenResult = Result<(Vec<Object>, Option<String>), String>;

impl Home {
    pub fn new(discovery: Discovery, kubectl: Option<PathBuf>, now: i64) -> Home {
        let conn = match (&discovery.target, &kubectl) {
            (None, _) => Conn::NoCluster,
            (Some(_), None) => Conn::NoKubectl,
            (Some(_), Some(_)) => Conn::Connecting,
        };
        Home { discovery, kubectl, conn, rows: Vec::new(), note: None, list: ListState::default(), now, rx: None, opening: None, open_note: None, browse: None }
    }

    /// Kick off the background fetch (no-op unless we have a target and kubectl).
    pub fn start(&mut self) {
        let (Some(t), Some(bin)) = (self.discovery.target.clone(), self.kubectl.clone()) else { return };
        let (tx, rx) = mpsc::channel();
        self.conn = Conn::Connecting;
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(cluster::fetch(&bin, &t));
        });
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.rx
            && let Ok(r) = rx.try_recv()
        {
            self.rx = None;
            self.apply(r);
        }
        if let Some((ns, rx)) = &self.opening
            && let Ok(r) = rx.try_recv()
        {
            let ns = ns.clone();
            self.opening = None;
            self.apply_open(&ns, r);
        }
    }

    /// Load the namespace's objects on a background thread, then open the explorer.
    pub fn open_namespace(&mut self, ns: &str) {
        let (Some(t), Some(bin)) = (self.discovery.target.clone(), self.kubectl.clone()) else { return };
        let (tx, rx) = mpsc::channel();
        let name = ns.to_string();
        self.open_note = None;
        self.opening = Some((name.clone(), rx));
        std::thread::spawn(move || {
            let _ = tx.send(cluster::fetch_objects(&bin, &t, &name));
        });
    }

    pub fn apply_open(&mut self, ns: &str, r: OpenResult) {
        match r {
            Err(e) => self.open_note = Some(format!("couldn't load {ns}: {e}")),
            Ok((objs, _)) if objs.is_empty() => self.open_note = Some(format!("no workloads found in namespace {ns}")),
            Ok((objs, note)) => {
                self.open_note = note;
                self.browse = Some(tui::App::new(objs, Filter::default(), self.now).with_picker(Some(ns.to_string())));
            }
        }
    }

    pub fn apply(&mut self, r: Result<Fetched, String>) {
        match r {
            Err(e) => {
                self.conn = Conn::Failed(e);
                self.rows.clear();
                self.list.select(None);
            }
            Ok(f) => {
                let current = self.discovery.target.as_ref().and_then(|t| t.namespace.clone());
                let mut all = f.namespaces;
                if all.is_empty() {
                    // Forbidden to list: still show where this context points.
                    let name = current.clone().unwrap_or_else(|| "default".into());
                    all.push(cluster::Namespace { name, phase: "Unknown".into(), created: None });
                }
                // Selected row follows the name across reloads.
                let keep = self.selected().map(|r| r.ns.name.clone());
                self.rows = cluster::order_namespaces(all, current.as_deref().or(Some("default")));
                let idx = keep.and_then(|k| self.rows.iter().position(|r| r.ns.name == k)).unwrap_or(0);
                self.list.select(Some(idx));
                self.note = f.note;
                self.conn = Conn::Connected { version: f.version };
            }
        }
    }

    pub fn selected(&self) -> Option<&NsRow> {
        self.rows.get(self.list.selected()?)
    }

    fn move_sel(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        let cur = self.list.selected().unwrap_or(0) as isize;
        self.list.select(Some((cur + d).clamp(0, max) as usize));
    }

    /// Returns true to quit.
    pub fn on_key(&mut self, k: KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return true;
        }
        if let Some(app) = &mut self.browse {
            // The explorer's "quit" means "back to the namespaces".
            if app.on_key(k) {
                self.browse = None;
            }
            return false;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Enter => {
                if let (Conn::Connected { .. }, None, Some(r)) = (&self.conn, &self.opening, self.selected()) {
                    let ns = r.ns.name.clone();
                    self.open_namespace(&ns);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Char('g') | KeyCode::Home => self.move_sel(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.move_sel(isize::MAX / 2),
            KeyCode::Char('r') if self.rx.is_none() => self.start(),
            _ => {}
        }
        false
    }

    /// Lines for the warning banner: discovery warnings plus runtime caveats.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = self.discovery.warnings.clone();
        if self.conn == Conn::NoKubectl {
            w.push("kubectl not found on PATH; scribe uses it to talk to the cluster".into());
        }
        if let Some(n) = &self.note {
            w.push(n.clone());
        }
        if let Some(n) = &self.open_note {
            w.push(n.clone());
        }
        w
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn source_label(t: &Target) -> String {
    match &t.source {
        Source::InCluster => "in-cluster".into(),
        Source::Kubeconfig(p) => p.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":"),
    }
}

pub fn draw(f: &mut Frame, h: &mut Home) {
    if let Some(app) = &mut h.browse {
        return tui::draw(f, app);
    }
    let warnings = h.warnings();
    let warn_h = if warnings.is_empty() { 0 } else { warnings.len().min(3) as u16 + 2 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Length(warn_h), Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    // -- header: bare-minimum cluster details, with placeholders when unknown
    let (badge, badge_style) = match &h.conn {
        Conn::Connected { .. } => ("● connected", Style::default().fg(Color::Green)),
        Conn::Connecting => ("◌ connecting…", Style::default().fg(Color::Yellow)),
        Conn::Failed(_) => ("✕ unreachable", Style::default().fg(Color::Red)),
        Conn::NoKubectl => ("✕ can't connect", Style::default().fg(Color::Red)),
        Conn::NoCluster => ("○ no cluster", dim()),
    };
    let v = |s: Option<&str>| Span::raw(s.unwrap_or("—").to_string());
    let t = h.discovery.target.as_ref();
    let version = match &h.conn {
        Conn::Connected { version } => version.clone(),
        _ => None,
    };
    let l1 = Line::from(vec![
        Span::styled(badge, badge_style.add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("server ", dim()),
        v(t.map(|t| t.server.as_str())),
        Span::styled("   version ", dim()),
        v(version.as_deref()),
    ]);
    let l2 = Line::from(vec![
        Span::styled("context ", dim()),
        v(t.and_then(|t| t.context.as_deref())),
        Span::styled("   cluster ", dim()),
        v(t.map(|t| t.cluster.as_str())),
        Span::styled("   user ", dim()),
        v(t.and_then(|t| t.user.as_deref())),
        Span::styled("   namespace ", dim()),
        v(t.and_then(|t| t.namespace.as_deref()).or(t.map(|_| "default"))),
    ]);
    let l3 = Line::from(vec![
        Span::styled("source ", dim()),
        v(t.map(source_label).as_deref()),
        Span::styled(t.filter(|t| t.contexts_total > 1).map_or(String::new(), |t| format!("   ({} contexts)", t.contexts_total)), dim()),
    ]);
    f.render_widget(Paragraph::new(vec![l1, l2, l3]).block(Block::default().borders(Borders::ALL).title(" scribe ")), rows[0]);

    if !warnings.is_empty() {
        let lines: Vec<Line> = warnings.iter().take(3).map(|w| Line::styled(format!("⚠ {w}"), Style::default().fg(Color::Yellow))).collect();
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow)).title(" warning ")),
            rows[1],
        );
    }

    // -- body
    let body = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(40), Constraint::Percentage(60)]).split(rows[2]);

    let items: Vec<ListItem> = match (&h.conn, h.rows.is_empty()) {
        (Conn::Connected { .. }, false) => h
            .rows
            .iter()
            .map(|r| {
                let (mark, style) = match r.kind {
                    NsKind::Current => ("★ ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    NsKind::User => ("  ", Style::default()),
                    NsKind::System => ("  ", dim()),
                };
                ListItem::new(Line::styled(format!("{mark}{}", r.ns.name), style))
            })
            .collect(),
        (Conn::Connecting, _) => vec![ListItem::new(Line::styled("  loading…", dim()))],
        (Conn::Failed(_), _) | (Conn::NoKubectl, _) => vec![ListItem::new(Line::styled("  — unavailable —", dim()))],
        _ => vec![ListItem::new(Line::styled("  — no cluster —", dim()))],
    };
    let title = if h.rows.is_empty() { " namespaces ".to_string() } else { format!(" namespaces ({}) ", h.rows.len()) };
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title)).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, body[0], &mut h.list);

    let detail = detail_lines(h);
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(" details ")), body[1]);

    let help = if h.rows.is_empty() { "r retry · q quit" } else { "j/k move · enter explore · g/G ends · r reload · q quit" };
    f.render_widget(Paragraph::new(Line::styled(help, dim())), rows[3]);
}

fn detail_lines(h: &Home) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match &h.conn {
        Conn::NoCluster => vec![
            Line::styled("No cluster to show yet.", bold),
            Line::raw(""),
            Line::raw("scribe looks, in order, at:"),
            Line::raw("  1. --kubeconfig <file>"),
            Line::raw("  2. $KUBECONFIG"),
            Line::raw("  3. ~/.kube/config"),
            Line::raw("  4. the in-cluster service account"),
            Line::raw(""),
            Line::raw("You can also read saved objects offline:"),
            Line::styled("  scribe object.yaml", Style::default().fg(Color::Cyan)),
            Line::styled("  kubectl get deploy x -o yaml --show-managed-fields | scribe", Style::default().fg(Color::Cyan)),
        ],
        Conn::NoKubectl => vec![
            Line::styled("Cluster found, but can't connect.", bold),
            Line::raw(""),
            Line::raw("scribe delegates authentication to kubectl for now. Install it or add it to PATH, then press r."),
        ],
        Conn::Connecting => vec![Line::styled("Connecting…", Style::default().fg(Color::Yellow))],
        Conn::Failed(e) => vec![
            Line::styled("Couldn't reach the cluster.", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Line::raw(""),
            Line::raw(e.clone()),
            Line::raw(""),
            Line::styled("Check the server address and your credentials, then press r.", dim()),
        ],
        Conn::Connected { .. } => match h.selected() {
            None => vec![Line::raw("no namespaces")],
            Some(r) => {
                let mut l = vec![Line::styled(r.ns.name.clone(), bold), Line::raw("")];
                l.push(Line::raw(format!("phase    {}", r.ns.phase)));
                l.push(Line::raw(format!("age      {}", r.ns.created.map_or("—".into(), |c| timeutil::human_age(h.now - c)))));
                l.push(Line::raw(format!(
                    "kind     {}",
                    match r.kind {
                        NsKind::Current => "default namespace of the active context",
                        NsKind::User => "workload namespace",
                        NsKind::System => "system namespace",
                    }
                )));
                l.push(Line::raw(""));
                match &h.opening {
                    Some((ns, _)) => l.push(Line::styled(format!("Loading objects in {ns}…"), Style::default().fg(Color::Yellow))),
                    None => l.push(Line::styled("enter  explore who wrote what in this namespace", Style::default().fg(Color::Cyan))),
                }
                l
            }
        },
    }
}

pub fn run(mut home: Home) -> Result<()> {
    home.start();
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            home.poll();
            terminal.draw(|f| draw(f, &mut home))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                    && k.kind == KeyEventKind::Press && home.on_key(k) {
                        return Ok(());
                    }
        }
    })();
    ratatui::restore();
    result
}

/// Plain-text summary for non-interactive use.
pub fn summary(d: &Discovery, kubectl_found: bool, fetched: Option<&Result<Fetched, String>>) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for w in &d.warnings {
        let _ = writeln!(s, "warning: {w}");
    }
    if d.target.is_some() && !kubectl_found {
        let _ = writeln!(s, "warning: kubectl not found on PATH; scribe uses it to talk to the cluster");
    }
    if let Some(t) = &d.target {
        let _ = writeln!(s, "context\t{}", t.context.as_deref().unwrap_or("-"));
        let _ = writeln!(s, "cluster\t{}", t.cluster);
        let _ = writeln!(s, "server\t{}", t.server);
        let _ = writeln!(s, "namespace\t{}", t.namespace.as_deref().unwrap_or("default"));
    }
    match fetched {
        Some(Ok(f)) => {
            let _ = writeln!(s, "version\t{}", f.version.as_deref().unwrap_or("-"));
            let cur = d.target.as_ref().and_then(|t| t.namespace.as_deref()).or(Some("default"));
            for r in cluster::order_namespaces(f.namespaces.clone(), cur) {
                let _ = writeln!(s, "ns\t{}\t{}", r.ns.name, r.ns.phase);
            }
        }
        Some(Err(e)) => {
            let _ = writeln!(s, "error: {e}");
        }
        None => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Namespace;
    use ratatui::{Terminal, backend::TestBackend};

    fn target() -> Target {
        Target {
            source: Source::Kubeconfig(vec!["/home/u/.kube/config".into()]),
            context: Some("kind-kind".into()),
            cluster: "kind-kind".into(),
            server: "https://127.0.0.1:6443".into(),
            user: Some("admin".into()),
            namespace: Some("team-a".into()),
            contexts_total: 3,
            kubeconfig_flag: None,
        }
    }
    fn ns(n: &str) -> Namespace {
        Namespace { name: n.into(), phase: "Active".into(), created: Some(1_000) }
    }
    fn screen(h: &mut Home) -> String {
        let mut t = Terminal::new(TestBackend::new(110, 24)).unwrap();
        t.draw(|f| draw(f, h)).unwrap();
        t.backend().to_string()
    }
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn connected() -> Home {
        let d = Discovery { target: Some(target()), warnings: vec![] };
        let mut h = Home::new(d, Some("/usr/bin/kubectl".into()), 100_000);
        h.apply(Ok(Fetched { version: Some("v1.31.0".into()), namespaces: vec![ns("kube-system"), ns("zeta"), ns("default"), ns("team-a")], note: None }));
        h
    }

    #[test]
    fn no_cluster_renders_placeholder_and_warning() {
        let d = Discovery { target: None, warnings: vec!["couldn't discover any k8s cluster (looked at: x)".into()] };
        let mut h = Home::new(d, None, 0);
        assert_eq!(h.conn, Conn::NoCluster);
        let s = screen(&mut h);
        assert!(s.contains("no cluster"), "{s}");
        assert!(s.contains("couldn't discover any k8s cluster"), "{s}");
        assert!(s.contains("--kubeconfig"), "{s}");
        assert!(s.contains("namespaces"), "{s}");
        assert!(!h.on_key(key(KeyCode::Char('j')))); // navigation on empty list is safe
        assert!(h.on_key(key(KeyCode::Char('q'))));
    }

    #[test]
    fn connected_shows_details_and_ordered_namespaces() {
        let mut h = connected();
        let s = screen(&mut h);
        for want in ["connected", "https://127.0.0.1:6443", "v1.31.0", "kind-kind", "admin", "(3 contexts)", "★ team-a", "namespaces (4)"] {
            assert!(s.contains(want), "missing {want:?}\n{s}");
        }
        let names: Vec<_> = h.rows.iter().map(|r| r.ns.name.as_str()).collect();
        assert_eq!(names, ["team-a", "default", "zeta", "kube-system"]);
        assert_eq!(h.selected().unwrap().ns.name, "team-a"); // current namespace preselected
    }

    #[test]
    fn navigation_clamps_and_reload_keeps_selection() {
        let mut h = connected();
        for _ in 0..10 {
            h.on_key(key(KeyCode::Char('j')));
        }
        assert_eq!(h.selected().unwrap().ns.name, "kube-system");
        h.on_key(key(KeyCode::Char('g')));
        assert_eq!(h.selected().unwrap().ns.name, "team-a");
        h.on_key(key(KeyCode::Char('j')));
        h.apply(Ok(Fetched { version: None, namespaces: vec![ns("default"), ns("team-a"), ns("new")], note: None }));
        assert_eq!(h.selected().unwrap().ns.name, "default");
    }

    #[test]
    fn failure_and_missing_kubectl_are_explained() {
        let d = Discovery { target: Some(target()), warnings: vec![] };
        let mut h = Home::new(d.clone(), Some("/k".into()), 0);
        assert_eq!(h.conn, Conn::Connecting);
        h.apply(Err("connection refused".into()));
        let s = screen(&mut h);
        assert!(s.contains("unreachable") && s.contains("connection refused"), "{s}");

        let mut h = Home::new(d, None, 0);
        let s = screen(&mut h);
        assert!(s.contains("can't connect") && s.contains("kubectl not found"), "{s}");
        h.on_key(key(KeyCode::Char('r'))); // no kubectl: must not panic or hang
        assert_eq!(h.conn, Conn::NoKubectl);
    }

    #[test]
    fn forbidden_namespace_listing_falls_back_to_context_namespace() {
        let d = Discovery { target: Some(target()), warnings: vec![] };
        let mut h = Home::new(d, Some("/k".into()), 0);
        h.apply(Ok(Fetched { version: None, namespaces: vec![], note: Some("not allowed to list namespaces".into()) }));
        assert_eq!(h.rows.len(), 1);
        assert_eq!(h.rows[0].ns.name, "team-a");
        assert!(screen(&mut h).contains("not allowed to list namespaces"));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut h = connected();
        for (w, hgt) in [(20, 5), (1, 1), (80, 8)] {
            let mut t = Terminal::new(TestBackend::new(w, hgt)).unwrap();
            t.draw(|f| draw(f, &mut h)).unwrap();
        }
    }

    fn workloads() -> Vec<Object> {
        crate::input::parse_documents(
            "kind: Deployment\nmetadata: {name: web, uid: d1, managedFields: [{manager: kubectl, operation: Update, time: '2024-01-01T00:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}]}\n---\nkind: ReplicaSet\nmetadata:\n  name: web-1\n  ownerReferences: [{kind: Deployment, name: web, uid: d1, controller: true}]\n  managedFields: [{manager: kcm, operation: Update, time: '2024-01-01T01:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}]\n",
        )
        .unwrap()
    }

    #[test]
    fn enter_opens_namespace_explorer_and_esc_returns() {
        let mut h = connected();
        h.apply_open("team-a", Ok((workloads(), None)));
        assert!(h.browse.is_some());
        let s = screen(&mut h);
        assert!(s.contains("scribe · team-a") && s.contains("Deployment/web") && s.contains("└ ReplicaSet/web-1"), "{s}");
        // Enter -> field browser on the Deployment; the authorship graph is shown
        assert!(!h.on_key(key(KeyCode::Enter)));
        let s = screen(&mut h);
        assert!(s.contains("spec.replicas") && s.contains("last write: kubectl"), "{s}");
        // Esc -> picker -> overview; the overview survives with its selection
        assert!(!h.on_key(key(KeyCode::Esc)));
        assert!(!h.on_key(key(KeyCode::Esc)));
        assert!(h.browse.is_none());
        assert_eq!(h.selected().unwrap().ns.name, "team-a");
        assert!(h.on_key(key(KeyCode::Esc))); // overview: quit
    }

    #[test]
    fn ctrl_c_quits_from_inside_the_explorer() {
        let mut h = connected();
        h.apply_open("team-a", Ok((workloads(), None)));
        assert!(h.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn open_failures_stay_on_overview_with_a_warning() {
        let mut h = connected();
        h.apply_open("team-a", Err("forbidden".into()));
        assert!(h.browse.is_none());
        assert!(screen(&mut h).contains("couldn't load team-a: forbidden"));
        h.apply_open("team-a", Ok((vec![], None)));
        assert!(screen(&mut h).contains("no workloads found in namespace team-a"));
        // partial results open the explorer and keep the caveat visible after returning
        h.apply_open("team-a", Ok((workloads(), Some("partial result: cronjobs is forbidden".into()))));
        assert!(h.browse.is_some());
        h.on_key(key(KeyCode::Esc));
        h.on_key(key(KeyCode::Esc));
        assert!(screen(&mut h).contains("partial result: cronjobs is forbidden"));
    }

    #[test]
    fn enter_without_connection_is_inert() {
        let d = Discovery { target: None, warnings: vec![] };
        let mut h = Home::new(d, None, 0);
        assert!(!h.on_key(key(KeyCode::Enter)));
        assert!(h.browse.is_none() && h.opening.is_none());
        let mut h = connected();
        h.kubectl = None;
        h.on_key(key(KeyCode::Enter)); // no kubectl: must not panic or start anything
        assert!(h.opening.is_none());
    }

    #[test]
    fn summary_text() {
        let d = Discovery { target: Some(target()), warnings: vec!["w1".into()] };
        let f = Ok(Fetched { version: Some("v1".into()), namespaces: vec![ns("b"), ns("team-a")], note: None });
        let s = summary(&d, true, Some(&f));
        assert!(summary(&d, false, None).contains("warning: kubectl not found"));
        assert!(s.starts_with("warning: w1\n") && s.contains("server\thttps://127.0.0.1:6443") && s.contains("ns\tteam-a\tActive\nns\tb"), "{s}");
    }
}
