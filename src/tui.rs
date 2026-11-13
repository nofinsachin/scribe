//! Interactive browser: field list, per-field authorship detail, owner-chain pivoting.

use crate::model::{Filter, Object};
use crate::{owners, timeutil};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Editing the field-path filter.
    Search,
}

pub struct App {
    pub objs: Vec<Object>,
    pub filter: Filter,
    pub cur: usize,
    /// Objects we pivoted away from, for `b`ack.
    pub history: Vec<usize>,
    pub visible: Vec<usize>,
    pub list: ListState,
    pub mode: Mode,
    pub by_manager: bool,
    pub now: i64,
    pub status: String,
    /// Object picker (owner tree) shown instead of the field browser.
    pub picker: bool,
    pub pick_query: String,
    pub pick_list: ListState,
    /// `(object index, depth)` of the rows currently shown in the picker.
    pub pick_rows: Vec<(usize, usize)>,
    /// What the objects were loaded from (e.g. a namespace), for the title.
    pub scope: Option<String>,
}

impl App {
    pub fn new(objs: Vec<Object>, filter: Filter, now: i64) -> App {
        let mut a = App {
            objs,
            filter,
            cur: 0,
            history: Vec::new(),
            visible: Vec::new(),
            list: ListState::default(),
            mode: Mode::Normal,
            by_manager: false,
            now,
            status: String::new(),
            picker: false,
            pick_query: String::new(),
            pick_list: ListState::default(),
            pick_rows: Vec::new(),
            scope: None,
        };
        a.refresh();
        a
    }

    /// Start in the object picker when there is more than one object to choose from.
    pub fn with_picker(mut self, scope: Option<String>) -> App {
        self.scope = scope;
        if self.objs.len() > 1 {
            self.open_picker();
        }
        self
    }

    pub fn open_picker(&mut self) {
        self.picker = true;
        self.mode = Mode::Normal;
        self.pick_query.clear();
        self.pick_rows = owners::tree_order(&self.objs);
        let at = self.pick_rows.iter().position(|r| r.0 == self.cur).unwrap_or(0);
        self.pick_list.select(Some(at));
    }

    /// Re-filter the picker, keeping the same object selected when it is still shown.
    pub fn refresh_picker(&mut self) {
        let keep = self.pick_selected();
        let q = self.pick_query.to_lowercase();
        self.pick_rows = owners::tree_order(&self.objs)
            .into_iter()
            .filter(|&(i, _)| q.is_empty() || self.objs[i].label().to_lowercase().contains(&q))
            .collect();
        let at = keep.and_then(|k| self.pick_rows.iter().position(|r| r.0 == k)).unwrap_or(0);
        self.pick_list.select(if self.pick_rows.is_empty() { None } else { Some(at) });
    }

    /// Object index of the highlighted picker row.
    pub fn pick_selected(&self) -> Option<usize> {
        Some(self.pick_rows.get(self.pick_list.selected()?)?.0)
    }

    fn move_pick(&mut self, delta: isize) {
        if self.pick_rows.is_empty() {
            return;
        }
        let max = self.pick_rows.len() as isize - 1;
        let cur = self.pick_list.selected().unwrap_or(0) as isize;
        self.pick_list.select(Some((cur + delta).clamp(0, max) as usize));
    }

    fn on_pick_key(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                if self.pick_query.is_empty() {
                    return true;
                }
                self.pick_query.clear();
                self.refresh_picker();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_pick(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_pick(-1),
            KeyCode::PageDown => self.move_pick(10),
            KeyCode::PageUp => self.move_pick(-10),
            KeyCode::Char('g') | KeyCode::Home => self.move_pick(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.move_pick(isize::MAX / 2),
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Enter => {
                if let Some(i) = self.pick_selected() {
                    self.picker = false;
                    self.history.clear();
                    self.filter.field = None;
                    self.filter.manager = None;
                    self.goto(i, false);
                }
            }
            _ => {}
        }
        false
    }

    /// Recompute the visible field set, keeping the selection in range.
    pub fn refresh(&mut self) {
        let o = &self.objs[self.cur];
        let mut v: Vec<usize> = (0..o.fields.len()).filter(|&i| self.filter.matches(&o.fields[i])).collect();
        if self.by_manager {
            v.sort_by(|&a, &b| {
                let (fa, fb) = (&o.fields[a], &o.fields[b]);
                fa.last().manager.cmp(&fb.last().manager).then_with(|| fa.path.cmp(&fb.path))
            });
        }
        self.visible = v;
        let sel = self.list.selected().unwrap_or(0).min(self.visible.len().saturating_sub(1));
        self.list.select(if self.visible.is_empty() { None } else { Some(sel) });
    }

    pub fn selected_field(&self) -> Option<&crate::model::Field> {
        let i = *self.visible.get(self.list.selected()?)?;
        self.objs[self.cur].fields.get(i)
    }

    fn goto(&mut self, idx: usize, remember: bool) {
        if remember && idx != self.cur {
            self.history.push(self.cur);
        }
        self.cur = idx;
        self.list.select(Some(0));
        self.refresh();
    }

    fn move_sel(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let max = self.visible.len() as isize - 1;
        let cur = self.list.selected().unwrap_or(0) as isize;
        self.list.select(Some((cur + delta).clamp(0, max) as usize));
    }

    /// Returns true when the app should quit.
    pub fn on_key(&mut self, k: KeyEvent) -> bool {
        self.status.clear();
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return true;
        }
        if self.mode == Mode::Search && self.picker {
            match k.code {
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Esc => {
                    self.pick_query.clear();
                    self.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    self.pick_query.pop();
                }
                KeyCode::Char(c) => self.pick_query.push(c),
                _ => {}
            }
            self.refresh_picker();
            return false;
        }
        if self.mode == Mode::Search {
            let text = self.filter.field.get_or_insert_with(String::new);
            match k.code {
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Esc => {
                    self.filter.field = None;
                    self.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            if self.filter.field.as_deref() == Some("") {
                self.filter.field = None;
            }
            self.list.select(Some(0));
            self.refresh();
            return false;
        }
        if self.picker {
            return self.on_pick_key(k);
        }
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                if self.filter.field.is_some() || self.filter.manager.is_some() {
                    self.filter.field = None;
                    self.filter.manager = None;
                    self.refresh();
                } else if self.objs.len() > 1 {
                    self.open_picker(); // step back: field browser -> object picker
                } else {
                    return true;
                }
            }
            KeyCode::Char('p') => {
                if self.objs.len() > 1 {
                    self.open_picker();
                } else {
                    self.status = "only one object loaded".into();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Char('g') | KeyCode::Home => self.list.select(if self.visible.is_empty() { None } else { Some(0) }),
            KeyCode::Char('G') | KeyCode::End => self.move_sel(isize::MAX / 2),
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.filter.field.get_or_insert_with(String::new);
            }
            KeyCode::Char('m') => {
                self.by_manager = !self.by_manager;
                self.refresh();
            }
            // Pivot: filter to everything the selected field's last writer touched.
            KeyCode::Char('M') => {
                if self.filter.manager.is_some() {
                    self.filter.manager = None;
                } else if let Some(f) = self.selected_field() {
                    self.filter.manager = Some(f.last().manager.clone());
                }
                self.list.select(Some(0));
                self.refresh();
            }
            KeyCode::Char('x') => {
                self.filter = Filter::default();
                self.refresh();
            }
            // Owner chain: up to the parent, down to the first child, back.
            KeyCode::Char('o') | KeyCode::Enter => {
                match owners::chain(&self.objs, self.cur).first() {
                    Some(l) => match l.resolved {
                        Some(i) => self.goto(i, true),
                        None => {
                            self.status = format!(
                                "owner {}/{} is not in the input; add its YAML to pivot",
                                l.reference.kind, l.reference.name
                            )
                        }
                    },
                    None => self.status = "no owner references".into(),
                }
            }
            KeyCode::Char('c') => match owners::children(&self.objs, self.cur).first() {
                Some(&i) => self.goto(i, true),
                None => self.status = "no children in the input".into(),
            },
            KeyCode::Char('b') | KeyCode::Backspace => {
                if let Some(i) = self.history.pop() {
                    self.goto(i, false);
                }
            }
            KeyCode::Tab => self.goto((self.cur + 1) % self.objs.len(), false),
            KeyCode::BackTab => self.goto((self.cur + self.objs.len() - 1) % self.objs.len(), false),
            _ => {}
        }
        false
    }
}

fn age(epoch: Option<i64>, now: i64) -> String {
    epoch.map_or("-".into(), |e| timeutil::human_age(now - e))
}

fn draw_picker(f: &mut Frame, app: &mut App) {
    let dim = Style::default().fg(Color::DarkGray);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(rows[1]);

    let newest = app.objs.iter().filter_map(|o| o.latest().map(|a| (a, o))).max_by_key(|(a, _)| a.epoch);
    let mut head = vec![Span::raw(format!("{} objects", app.objs.len()))];
    if let Some((a, o)) = newest {
        head.push(Span::styled("   most recent write ", dim));
        head.push(Span::styled(a.manager.clone(), Style::default().fg(Color::Green)));
        head.push(Span::styled(format!(" {} ago", age(a.epoch, app.now)), Style::default().fg(Color::Yellow)));
        head.push(Span::styled(format!(" on {}/{}", o.kind, o.name), dim));
    }
    let title = match &app.scope {
        Some(s) => format!(" scribe · {s} "),
        None => " scribe ".to_string(),
    };
    f.render_widget(Paragraph::new(Line::from(head)).block(Block::default().borders(Borders::ALL).title(title)), rows[0]);

    let label = |o: &Object| format!("{}/{}", o.kind, o.name);
    let avail = (body[0].width as usize).saturating_sub(2 + 2 + 30).max(12);
    let width = app
        .pick_rows
        .iter()
        .map(|&(i, d)| d * 2 + label(&app.objs[i]).chars().count())
        .max()
        .unwrap_or(10)
        .min(avail);
    let items: Vec<ListItem> = app
        .pick_rows
        .iter()
        .map(|&(i, depth)| {
            let o = &app.objs[i];
            let mut text = format!("{}{}", if depth > 0 { format!("{}└ ", "  ".repeat(depth - 1)) } else { String::new() }, label(o));
            if text.chars().count() > width {
                text = text.chars().take(width.saturating_sub(1)).collect::<String>() + "…";
            }
            let pad = width.saturating_sub(text.chars().count());
            let mut spans = vec![Span::raw(text), Span::raw(" ".repeat(pad + 2))];
            match o.latest() {
                Some(a) => {
                    spans.push(Span::styled(a.manager.clone(), Style::default().fg(Color::Green)));
                    spans.push(Span::styled(format!("  {}", age(a.epoch, app.now)), Style::default().fg(Color::Yellow)));
                }
                None => spans.push(Span::styled("no managedFields", dim)),
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let shown = if app.pick_rows.len() == app.objs.len() { format!("{}", app.objs.len()) } else { format!("{}/{}", app.pick_rows.len(), app.objs.len()) };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(" objects ({shown}) ")))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, body[0], &mut app.pick_list);

    let detail: Vec<Line> = match app.pick_selected() {
        None => vec![Line::from("no matching objects")],
        Some(i) => {
            let o = &app.objs[i];
            let mut l = vec![Line::styled(o.label(), Style::default().add_modifier(Modifier::BOLD)), Line::styled(o.api_version.clone(), dim), Line::raw("")];
            let chain = owners::chain(&app.objs, i);
            if chain.is_empty() {
                l.push(Line::raw("owned by   (nobody)"));
            }
            for link in &chain {
                let name = format!("{}/{}", link.reference.kind, link.reference.name);
                l.push(Line::from(vec![
                    Span::styled("owned by   ", dim),
                    Span::styled(name, if link.resolved.is_some() { Style::default().fg(Color::Cyan) } else { dim }),
                ]));
            }
            l.push(Line::raw(format!("owns       {} loaded", owners::children(&app.objs, i).len())));
            l.push(Line::raw(format!("fields     {} (from {} managedFields entries)", o.fields.len(), o.entry_count)));
            l.push(Line::raw(""));
            match o.latest() {
                Some(a) => {
                    l.push(Line::from(vec![Span::styled("last write ", dim), Span::styled(a.manager.clone(), Style::default().fg(Color::Green))]));
                    l.push(Line::raw(format!("           {} ({} ago)", a.time.as_deref().unwrap_or("-"), age(a.epoch, app.now))));
                }
                None => l.push(Line::styled("no managedFields on this object", dim)),
            }
            l.push(Line::raw(""));
            l.push(Line::styled("enter to explore who wrote each field", dim));
            l
        }
    };
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(" object ")), body[1]);

    let foot = if app.mode == Mode::Search {
        Line::from(vec![Span::styled("/", Style::default().fg(Color::Yellow)), Span::raw(app.pick_query.clone()), Span::raw("█")])
    } else if !app.pick_query.is_empty() {
        Line::styled(format!("filter “{}” · esc clears · enter open", app.pick_query), Style::default().fg(Color::Yellow))
    } else {
        Line::styled("j/k move · enter open · / find · esc back · q quit", dim)
    };
    f.render_widget(Paragraph::new(foot), rows[2]);
}

pub fn draw(f: &mut Frame, app: &mut App) {
    if app.picker {
        return draw_picker(f, app);
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(rows[1]);

    let obj = &app.objs[app.cur];
    let chain = owners::chain(&app.objs, app.cur);
    let mut chain_spans = vec![Span::styled("owner chain: ", Style::default().fg(Color::DarkGray))];
    if chain.is_empty() {
        chain_spans.push(Span::raw("(none)"));
    }
    for (i, l) in chain.iter().enumerate() {
        if i > 0 {
            chain_spans.push(Span::raw(" → "));
        }
        let style = if l.resolved.is_some() { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };
        chain_spans.push(Span::styled(format!("{}/{}", l.reference.kind, l.reference.name), style));
    }
    let kids = owners::children(&app.objs, app.cur).len();
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(obj.label(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("   {} entries · {} fields · object {}/{} · {} children", obj.entry_count, obj.fields.len(), app.cur + 1, app.objs.len(), kids)),
        ]),
        Line::from(chain_spans),
    ])
    .block(Block::default().borders(Borders::ALL).title(" scribe "));
    f.render_widget(header, rows[0]);

    // Leave room for "  manager  age  +N" (~34 cols) so the writer is never clipped.
    let avail = (body[0].width as usize).saturating_sub(2 + 34).max(12);
    let width = app.visible.iter().map(|&i| obj.fields[i].path.chars().count()).max().unwrap_or(5).min(avail);
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| {
            let fld = &obj.fields[i];
            let a = fld.last();
            let mut path: String = if fld.path.chars().count() > width {
                // Keep the tail: the leaf name is the informative part.
                let tail: String = fld.path.chars().rev().take(width - 1).collect::<Vec<_>>().into_iter().rev().collect();
                format!("…{tail}")
            } else {
                fld.path.clone()
            };
            while path.chars().count() < width {
                path.push(' ');
            }
            let mut spans = vec![
                Span::raw(path),
                Span::raw("  "),
                Span::styled(a.manager.clone(), Style::default().fg(Color::Green)),
                Span::styled(format!("  {}", age(a.epoch, app.now)), Style::default().fg(Color::Yellow)),
            ];
            if fld.actors.len() > 1 {
                spans.push(Span::styled(format!("  +{}", fld.actors.len() - 1), Style::default().fg(Color::Magenta)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = format!(" fields ({}{}) ", app.visible.len(), if app.by_manager { ", by manager" } else { "" });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, body[0], &mut app.list);

    let detail: Vec<Line> = match app.selected_field() {
        None => vec![Line::from("no matching fields")],
        Some(fld) => {
            let mut l = vec![Line::styled(fld.path.clone(), Style::default().add_modifier(Modifier::BOLD)), Line::raw("")];
            for (n, a) in fld.actors.iter().enumerate() {
                let tag = if n == 0 { "last write" } else { "also owned" };
                l.push(Line::from(vec![
                    Span::styled(format!("{tag}: "), Style::default().fg(Color::DarkGray)),
                    Span::styled(a.manager.clone(), Style::default().fg(Color::Green)),
                ]));
                let mut meta = format!("  {}", a.operation);
                if let Some(s) = &a.subresource {
                    meta += &format!(" · subresource {s}");
                }
                l.push(Line::raw(meta));
                l.push(Line::raw(format!("  {} ({} ago)", a.time.as_deref().unwrap_or("no timestamp"), age(a.epoch, app.now))));
                l.push(Line::raw(""));
            }
            l
        }
    };
    f.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(" authorship ")),
        body[1],
    );

    let foot = if app.mode == Mode::Search {
        Line::from(vec![Span::styled("/", Style::default().fg(Color::Yellow)), Span::raw(app.filter.field.clone().unwrap_or_default()), Span::raw("█")])
    } else if !app.status.is_empty() {
        Line::styled(app.status.clone(), Style::default().fg(Color::Yellow))
    } else {
        let mut s = String::from("j/k move · / find · m sort · M manager · o owner · c child · b back · p picker · x clear · esc back");
        if let Some(m) = &app.filter.manager {
            s = format!("manager={m} · {s}");
        }
        Line::styled(s, Style::default().fg(Color::DarkGray))
    };
    f.render_widget(Paragraph::new(foot), rows[2]);
}

pub fn run(objs: Vec<Object>, filter: Filter) -> Result<()> {
    let mut app = App::new(objs, filter, timeutil::now_secs()).with_picker(None);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| draw(f, &mut app))?;
            if let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press && app.on_key(k) {
                    return Ok(());
                }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::parse_documents;
    use ratatui::{Terminal, backend::TestBackend};

    const Y: &str = "\
kind: Deployment
metadata:
  name: web
  uid: d1
  managedFields:
    - {manager: kubectl, operation: Update, time: '2024-01-01T00:00:00Z', fieldsV1: {f:spec: {f:replicas: {}, f:paused: {}}}}
    - {manager: hpa, operation: Update, subresource: scale, time: '2024-01-01T01:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}
---
kind: ReplicaSet
metadata:
  name: rs
  uid: r1
  ownerReferences: [{kind: Deployment, name: web, uid: d1, controller: true}]
  managedFields:
    - {manager: kcm, operation: Update, time: '2024-01-01T00:00:00Z', fieldsV1: {f:spec: {f:replicas: {}}}}
";

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn app() -> App {
        App::new(parse_documents(Y).unwrap(), Filter::default(), 1_704_078_000)
    }

    fn screen(app: &mut App) -> String {
        let mut t = Terminal::new(TestBackend::new(110, 20)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().to_string()
    }

    #[test]
    fn renders_fields_and_detail() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('j')));
        let s = screen(&mut a);
        assert!(s.contains("Deployment/web"), "{s}");
        assert!(s.contains("spec.replicas"), "{s}");
        assert!(s.contains("last write: hpa"), "{s}");
        assert!(s.contains("also owned: kubectl"), "{s}");
    }

    #[test]
    fn navigation_and_search() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.selected_field().unwrap().path, "spec.replicas");
        a.on_key(key(KeyCode::Char('/')));
        for c in "pau".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.visible.len(), 1);
        assert!(!a.on_key(key(KeyCode::Esc))); // clears filter first
        assert_eq!(a.visible.len(), 2);
        assert!(!a.on_key(key(KeyCode::Esc))); // then steps back to the picker (2 objects)
        assert!(a.picker);
        assert!(a.on_key(key(KeyCode::Esc))); // then leaves
    }

    #[test]
    fn owner_pivot_and_back() {
        let mut a = app();
        a.on_key(key(KeyCode::Tab)); // ReplicaSet
        assert_eq!(a.cur, 1);
        a.on_key(key(KeyCode::Char('o')));
        assert_eq!(a.cur, 0);
        a.on_key(key(KeyCode::Char('c')));
        assert_eq!(a.cur, 1);
        a.on_key(key(KeyCode::Char('b')));
        assert_eq!(a.cur, 0);
        a.on_key(key(KeyCode::Char('o'))); // root: nothing to pivot to
        assert_eq!(a.cur, 0);
        assert_eq!(a.status, "no owner references");
    }

    #[test]
    fn manager_pivot_toggles() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('j'))); // spec.replicas -> hpa
        a.on_key(key(KeyCode::Char('M')));
        assert_eq!(a.filter.manager.as_deref(), Some("hpa"));
        assert_eq!(a.visible.len(), 1);
        a.on_key(key(KeyCode::Char('M')));
        assert_eq!(a.visible.len(), 2);
    }

    #[test]
    fn long_paths_never_hide_the_writer() {
        let long = "a".repeat(200);
        let y = format!(
            "kind: Pod\nmetadata:\n  name: p\n  managedFields:\n    - {{manager: writer-x, operation: Apply, time: '2024-01-01T00:00:00Z', fieldsV1: {{f:{long}: {{}}}}}}\n"
        );
        let mut a = App::new(parse_documents(&y).unwrap(), Filter::default(), 1_704_078_000);
        let s = screen(&mut a);
        let line = s.lines().find(|l| l.contains('…')).expect("truncated row");
        assert!(line.contains("writer-x"), "{line}");
    }

    fn picker_app() -> App {
        App::new(parse_documents(Y).unwrap(), Filter::default(), 1_704_078_000).with_picker(Some("prod".into()))
    }

    #[test]
    fn picker_starts_for_multiple_objects_and_shows_owner_tree() {
        let mut a = picker_app();
        assert!(a.picker);
        assert_eq!(a.pick_rows, vec![(0, 0), (1, 1)]);
        let s = screen(&mut a);
        for want in ["scribe · prod", "2 objects", "most recent write hpa", "Deployment/web", "└ ReplicaSet/rs", "objects (2)", "kcm"] {
            assert!(s.contains(want), "missing {want:?}\n{s}");
        }
        // single object: no picker
        let one = App::new(parse_documents("kind: Pod\nmetadata: {name: p}\n").unwrap(), Filter::default(), 0).with_picker(None);
        assert!(!one.picker);
    }

    #[test]
    fn picker_open_and_step_back() {
        let mut a = picker_app();
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Enter));
        assert!(!a.picker);
        assert_eq!(a.cur, 1); // ReplicaSet
        assert!(screen(&mut a).contains("ReplicaSet/rs (default)") || screen(&mut a).contains("ReplicaSet/rs"));
        a.on_key(key(KeyCode::Char('o'))); // pivot to the Deployment
        assert_eq!(a.cur, 0);
        a.on_key(key(KeyCode::Char('p'))); // picker preselects the current object
        assert!(a.picker && a.pick_selected() == Some(0));
        assert!(!a.on_key(key(KeyCode::Char('j'))));
        assert!(a.on_key(key(KeyCode::Esc))); // picker is the top level: leave
    }

    #[test]
    fn picker_search_filters_and_keeps_selection_safe() {
        let mut a = picker_app();
        a.on_key(key(KeyCode::Char('/')));
        for c in "repl".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.pick_rows.len(), 1);
        assert_eq!(a.pick_selected(), Some(1));
        assert!(!a.on_key(key(KeyCode::Esc))); // clears the query first
        assert_eq!(a.pick_rows.len(), 2);
        a.on_key(key(KeyCode::Char('/')));
        for c in "zzz".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        assert!(a.pick_selected().is_none());
        a.on_key(key(KeyCode::Enter));
        a.on_key(key(KeyCode::Enter)); // nothing selected: must not panic or leave the picker
        assert!(a.picker);
        assert!(screen(&mut a).contains("no matching objects"));
    }

    #[test]
    fn esc_in_fields_goes_back_to_picker_not_quit() {
        let mut a = picker_app();
        a.on_key(key(KeyCode::Enter));
        assert!(!a.picker);
        assert!(!a.on_key(key(KeyCode::Esc)));
        assert!(a.picker);
        let mut single = App::new(parse_documents("kind: Pod\nmetadata: {name: p}\n").unwrap(), Filter::default(), 0);
        single.on_key(key(KeyCode::Char('p')));
        assert_eq!(single.status, "only one object loaded");
        assert!(single.on_key(key(KeyCode::Esc)));
    }

    #[test]
    fn empty_filter_result_is_safe() {
        let mut a = app();
        a.filter.field = Some("zzz".into());
        a.refresh();
        assert!(a.selected_field().is_none());
        a.on_key(key(KeyCode::Char('j')));
        let s = screen(&mut a);
        assert!(s.contains("no matching fields"), "{s}");
    }
}
