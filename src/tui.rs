//! The interactive view: what the macOS menu bar popover shows, in a terminal.

use std::collections::HashSet;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::scanner::{self, Proc, RunningSession, SessionGroup, Status};
use crate::store::{self, format_bytes, format_limit, human_age, plural, Config, KillReport, ScanResult, Target, MINIMUM_IDLE_LIMIT};

const ACCENT: Color = Color::Rgb(232, 98, 80);
const DIM: Color = Color::DarkGray;

enum Msg {
    Scanned(ScanResult),
    Killed { targets: Vec<u32>, report: KillReport },
}

#[derive(Clone, PartialEq)]
enum RowKind {
    Section,
    Note,
    Orphan(usize),
    OrphanProc(usize, usize),
    Session(usize),
    SessionProc(usize, usize),
    Live(usize),
    LiveProc(usize, usize),
}

struct Row {
    kind: RowKind,
    /// Identifies the row across scans, so the selection stays put when the list changes.
    key: String,
    line: Line<'static>,
}

enum Mode {
    Normal,
    Confirm { question: String, targets: Vec<Target> },
    Settings { field: usize, editing: Option<String> },
    Help,
}

struct App {
    cfg: Config,
    result: Option<ScanResult>,
    scanning: bool,
    rescan_when_done: bool,
    last_scan: Option<Instant>,
    killing: HashSet<u32>,
    /// What the most recent kills released; kills in quick succession add up into one message.
    freed: Option<(u64, usize, Instant)>,
    last_auto_end: Option<(usize, SystemTime)>,
    message: Option<(String, Instant)>,
    state: ListState,
    selected_key: Option<String>,
    mode: Mode,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
}

pub fn run(cfg: &mut Config) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        cfg: cfg.clone(),
        result: None,
        scanning: false,
        rescan_when_done: false,
        last_scan: None,
        killing: HashSet::new(),
        freed: None,
        last_auto_end: None,
        message: None,
        state: ListState::default(),
        selected_key: None,
        mode: Mode::Normal,
        tx,
        rx,
        quit: false,
    };
    app.refresh();
    let outcome = app.event_loop(&mut terminal);
    ratatui::restore();
    *cfg = app.cfg;
    outcome
}

impl App {
    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.quit {
            while let Ok(msg) = self.rx.try_recv() {
                self.handle(msg);
            }
            let due = self.last_scan.is_some_and(|t| t.elapsed() >= Duration::from_secs(self.cfg.refresh_interval));
            if due && !self.scanning {
                self.refresh();
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    // Windows reports key releases too.
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key);
                    }
                }
            }
        }
        Ok(())
    }

    fn refresh(&mut self) {
        if self.scanning {
            // A scan is already running and may predate a kill; run another when it ends.
            self.rescan_when_done = true;
            return;
        }
        self.scanning = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Scanned(store::full_scan()));
        });
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Scanned(result) => {
                self.result = Some(result);
                self.scanning = false;
                self.last_scan = Some(Instant::now());
                if self.cfg.ends_idle_sessions {
                    let due: Vec<RunningSession> = self
                        .result
                        .as_ref()
                        .map(|r| r.idle_sessions(&self.cfg))
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|s| !self.killing.contains(&s.process.pid))
                        .collect();
                    if !due.is_empty() {
                        self.last_auto_end = Some((due.len(), SystemTime::now()));
                        let targets = self.session_targets(&due);
                        self.kill(targets);
                    }
                }
                if self.rescan_when_done {
                    self.rescan_when_done = false;
                    self.refresh();
                }
            }
            Msg::Killed { targets, report } => {
                for pid in &targets {
                    self.killing.remove(pid);
                }
                if !report.gone.is_empty() {
                    let (bytes, count) = match self.freed {
                        Some((b, c, t)) if t.elapsed() < Duration::from_secs(15) => (b, c),
                        _ => (0, 0),
                    };
                    self.freed = Some((bytes + report.freed, count + report.all_gone.len(), Instant::now()));
                }
                if !report.survivors.is_empty() {
                    self.say(format!(
                        "{} could not be ended (it may be elevated or another user's)",
                        plural(report.survivors.len(), "process")
                    ));
                }
                // Take the dead ones off the list right away; the rescan then confirms it.
                let gone: HashSet<u32> = report.all_gone.into_iter().chain(report.gone).collect();
                if let Some(r) = &mut self.result {
                    r.remove(&gone);
                }
                self.refresh();
            }
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    fn kill(&mut self, targets: Vec<Target>) {
        let targets: Vec<Target> = targets.into_iter().filter(|t| !self.killing.contains(&t.pid)).collect();
        if targets.is_empty() {
            return;
        }
        self.killing.extend(targets.iter().map(|t| t.pid));
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        thread::spawn(move || {
            let report = store::kill(&targets, &cfg, Duration::from_secs(2));
            let _ = tx.send(Msg::Killed { targets: targets.iter().map(|t| t.pid).collect(), report });
        });
    }

    fn unprotected(&self, procs: &[Proc]) -> Vec<Target> {
        procs.iter().filter(|p| !self.cfg.is_protected(&p.command)).map(Target::from).collect()
    }

    /// Ending a session: the Claude Code process and what it has running, except protected processes.
    fn session_targets(&self, sessions: &[RunningSession]) -> Vec<Target> {
        sessions
            .iter()
            .flat_map(|s| std::iter::once(Target::from(&s.process)).chain(self.unprotected(&s.procs)))
            .collect()
    }

    fn save(&mut self) {
        if let Err(e) = self.cfg.save() {
            self.say(format!("Could not save settings: {e}"));
        }
    }

    // MARK: rows

    fn rows(&self, width: u16) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(r) = &self.result else {
            rows.push(Row { kind: RowKind::Note, key: "scanning".into(), line: Line::from("  Looking…".fg(DIM)) });
            return rows;
        };
        let width = width as usize;
        let now = scanner::now_ms();

        if r.orphans.is_empty() {
            rows.push(section("LEFT BEHIND", String::new()));
            rows.push(Row { kind: RowKind::Note, key: "clean".into(), line: Line::from("  Nothing left behind. All clean.".fg(Color::Green)) });
        } else {
            rows.push(section(
                "LEFT BEHIND",
                format!(
                    "{} from {} · {}",
                    plural(r.orphan_count(), "process"),
                    plural(r.orphans.len(), "closed session"),
                    format_bytes(r.orphan_memory())
                ),
            ));
            for (g, group) in r.orphans.iter().enumerate() {
                rows.push(self.group_row(RowKind::Orphan(g), group, width));
                for (p, proc) in group.procs.iter().enumerate() {
                    rows.push(self.proc_row(RowKind::OrphanProc(g, p), proc, width));
                }
            }
        }

        rows.push(Row { kind: RowKind::Note, key: "gap1".into(), line: Line::default() });
        let idle_count = r.idle_sessions(&self.cfg).len();
        let summary = if r.sessions.is_empty() {
            String::new()
        } else if idle_count > 0 {
            format!("{} · {} idle over {}", r.sessions.len(), idle_count, format_limit(self.cfg.idle_limit_minutes))
        } else {
            r.sessions.len().to_string()
        };
        rows.push(section("OPEN SESSIONS", summary));
        if r.sessions.is_empty() {
            rows.push(Row { kind: RowKind::Note, key: "nosessions".into(), line: Line::from("  No Claude Code session is open.".fg(DIM)) });
        }
        for (s, session) in r.sessions.iter().enumerate() {
            rows.push(self.session_row(s, session, now, width));
            for (p, proc) in session.procs.iter().enumerate() {
                rows.push(self.proc_row(RowKind::SessionProc(s, p), proc, width));
            }
        }

        if !r.live.is_empty() {
            rows.push(Row { kind: RowKind::Note, key: "gap2".into(), line: Line::default() });
            rows.push(section("RUNNING IN OTHER SESSIONS", String::new()));
            for (g, group) in r.live.iter().enumerate() {
                rows.push(self.group_row(RowKind::Live(g), group, width));
                for (p, proc) in group.procs.iter().enumerate() {
                    rows.push(self.proc_row(RowKind::LiveProc(g, p), proc, width));
                }
            }
        }
        rows
    }

    fn group_row(&self, kind: RowKind, group: &SessionGroup, width: usize) -> Row {
        let right = format!("{:>12}{:>10}{:>6} ", plural(group.procs.len(), "proc"), format_bytes(group.memory()), human_age(group.oldest()));
        let mut left = vec![Span::styled(format!(" {}", group.project), Style::new().bold())];
        if let Some(title) = &group.title {
            left.push(Span::raw(format!(" — {title}")));
        }
        left.push(Span::styled(format!("  {}", group.short_id()), Style::new().fg(DIM)));
        let busy = group.procs.iter().any(|p| self.killing.contains(&p.pid));
        let right_style = if busy { Style::new().fg(ACCENT) } else { Style::new() };
        let right = if busy { format!("{:>28} ", "killing…") } else { right };
        Row { key: format!("{kind_key}:{}", group.id, kind_key = kind_prefix(&kind)), kind, line: fit(left, Span::styled(right, right_style), width) }
    }

    fn proc_row(&self, kind: RowKind, proc: &Proc, width: usize) -> Row {
        let protected = self.cfg.is_protected(&proc.command);
        let ports = proc.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ");
        let killing = self.killing.contains(&proc.pid);
        let right = if killing {
            format!("{:>28} ", "killing…")
        } else {
            format!("{:>12}{:>10}{:>6} ", ports, format_bytes(proc.memory), human_age(proc.age))
        };
        let mut left = vec![Span::raw("     "), Span::styled(proc.short_command(), if protected { Style::new().fg(DIM) } else { Style::new() })];
        if protected {
            left.push(Span::styled("  protected", Style::new().fg(Color::Cyan)));
        }
        if !proc.helpers.is_empty() {
            left.push(Span::styled(format!("  +{}", proc.helpers.len()), Style::new().fg(DIM)));
        }
        let style = if killing { Style::new().fg(ACCENT) } else { Style::new().fg(Color::Gray) };
        Row { key: format!("{}:{}:{}", kind_prefix(&kind), proc.pid, proc.created), kind, line: fit(left, Span::styled(right, style), width) }
    }

    fn session_row(&self, s: usize, session: &RunningSession, now: f64, width: usize) -> Row {
        let limit = self.cfg.idle_limit_minutes * 60;
        let (dot, status) = match (&session.status, session.idle_seconds(now)) {
            (Status::Idle, Some(idle)) if idle >= limit => (Span::styled(" ●", Style::new().fg(ACCENT)), format!("idle {}", human_age(idle))),
            (Status::Idle, Some(idle)) => (Span::styled(" ●", Style::new().fg(Color::Yellow)), format!("idle {}", human_age(idle))),
            (Status::Idle, None) => (Span::styled(" ●", Style::new().fg(Color::Yellow)), "idle".into()),
            (Status::Busy, _) => (Span::styled(" ●", Style::new().fg(Color::Green)), "working".into()),
            (Status::Other(o), _) => (Span::styled(" ●", Style::new().fg(DIM)), o.clone()),
            (Status::Unknown, _) => (Span::styled(" ○", Style::new().fg(DIM)), "open".into()),
        };
        let mut left = vec![dot, Span::styled(format!(" {status:<9} "), Style::new().fg(DIM)), Span::styled(session.project.clone(), Style::new().bold())];
        if let Some(title) = &session.title {
            left.push(Span::raw(format!(" — {title}")));
        }
        let killing = self.killing.contains(&session.process.pid);
        let right = if killing {
            Span::styled(format!("{:>28} ", "ending…"), Style::new().fg(ACCENT))
        } else {
            Span::raw(format!("{:>12}{:>10}{:>6} ", format!("pid {}", session.process.pid), format_bytes(session.memory()), human_age(session.process.age)))
        };
        Row { key: format!("s:{}:{}", session.process.pid, session.process.created), kind: RowKind::Session(s), line: fit(left, right, width) }
    }

    fn selectable(kind: &RowKind) -> bool {
        !matches!(kind, RowKind::Section | RowKind::Note)
    }

    // MARK: drawing

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, details, footer] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(4), Constraint::Length(5), Constraint::Length(1)]).areas(f.area());

        let rows = self.rows(body.width);
        // Keep the same row selected across scans.
        let mut index = self.selected_key.as_ref().and_then(|k| rows.iter().position(|r| &r.key == k));
        if index.is_none() {
            let start = self.state.selected().unwrap_or(0).min(rows.len().saturating_sub(1));
            index = (start..rows.len()).chain((0..start).rev()).find(|&i| Self::selectable(&rows[i].kind));
        }
        self.state.select(index);
        self.selected_key = index.map(|i| rows[i].key.clone());

        self.draw_header(f, header);
        let items: Vec<ListItem> = rows.iter().map(|r| ListItem::new(r.line.clone())).collect();
        let list = List::new(items).highlight_style(Style::new().bg(Color::Rgb(50, 50, 60)));
        f.render_stateful_widget(list, body, &mut self.state);
        let selected = index.map(|i| rows[i].kind.clone());
        self.draw_details(f, details, selected.as_ref());
        self.draw_footer(f, footer);

        match &self.mode {
            Mode::Normal => {}
            Mode::Confirm { question, .. } => draw_confirm(f, question),
            Mode::Settings { field, editing } => self.draw_settings(f, *field, editing.as_deref()),
            Mode::Help => draw_help(f),
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let doll = match &self.result {
            Some(r) if r.orphan_count() > 0 => Span::styled(" ◐ seshka", Style::new().fg(ACCENT).bold()),
            Some(_) => Span::styled(" ● seshka", Style::new().fg(Color::Green).bold()),
            None => Span::styled(" ○ seshka", Style::new().bold()),
        };
        let mut spans = vec![doll, Span::styled("  kill what Claude Code left behind", Style::new().fg(DIM))];
        let status = if self.scanning {
            "scanning…".to_string()
        } else if let Some(t) = self.last_scan {
            format!("scanned {} ago · every {}s", human_age(t.elapsed().as_secs()), self.cfg.refresh_interval)
        } else {
            String::new()
        };
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = (area.width as usize).saturating_sub(used + status.chars().count() + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(status, Style::new().fg(DIM)));
        let mut lines = vec![Line::from(spans)];

        let mut second = Vec::new();
        if let Some((bytes, count, at)) = self.freed {
            if at.elapsed() < Duration::from_secs(15) {
                second.push(Span::styled(format!(" Freed {}", format_bytes(bytes)), Style::new().fg(Color::Green).bold()));
                second.push(Span::raw(format!(" · {} killed", plural(count, "process"))));
            }
        }
        if second.is_empty() {
            if let Some((count, at)) = self.last_auto_end {
                let ago = at.elapsed().map(|d| human_age(d.as_secs())).unwrap_or_default();
                second.push(Span::styled(format!(" Ended {} idle automatically, {ago} ago", plural(count, "session")), Style::new().fg(DIM)));
            }
        }
        lines.push(Line::from(second));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn selected_proc(&self, kind: &RowKind) -> Option<&Proc> {
        let r = self.result.as_ref()?;
        match *kind {
            RowKind::OrphanProc(g, p) => r.orphans.get(g)?.procs.get(p),
            RowKind::LiveProc(g, p) => r.live.get(g)?.procs.get(p),
            RowKind::SessionProc(s, p) => r.sessions.get(s)?.procs.get(p),
            RowKind::Session(s) => Some(&r.sessions.get(s)?.process),
            _ => None,
        }
    }

    fn draw_details(&self, f: &mut Frame, area: Rect, kind: Option<&RowKind>) {
        let block = Block::new().borders(Borders::TOP).border_style(Style::new().fg(DIM));
        let mut lines: Vec<Line> = Vec::new();
        let label = |s: &str| Span::styled(format!("{s:<8}"), Style::new().fg(DIM));
        if let Some(kind) = kind {
            if let Some(p) = self.selected_proc(kind) {
                let mut first = vec![label("pid"), Span::raw(p.pid.to_string())];
                if !p.helpers.is_empty() {
                    first.push(Span::styled(
                        format!("  + {} helper{}", p.helpers.len(), if p.helpers.len() == 1 { "" } else { "s" }),
                        Style::new().fg(DIM),
                    ));
                }
                first.push(Span::styled(format!("   started {} ago", human_age(p.age)), Style::new().fg(DIM)));
                if !p.ports.is_empty() {
                    first.push(Span::raw(format!("   ports {}", p.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "))));
                }
                lines.push(Line::from(first));
                lines.push(Line::from(vec![label("cwd"), Span::raw(p.cwd.clone())]));
                lines.push(Line::from(vec![label("command"), Span::raw(p.command.replace(['\n', '\r'], " "))]));
            } else if let Some(r) = &self.result {
                let group = match *kind {
                    RowKind::Orphan(g) => r.orphans.get(g),
                    RowKind::Live(g) => r.live.get(g),
                    _ => None,
                };
                if let Some(g) = group {
                    lines.push(Line::from(vec![label("session"), Span::raw(g.id.clone())]));
                    lines.push(Line::from(vec![label("project"), Span::raw(g.project.clone())]));
                    if let Some(t) = &g.title {
                        lines.push(Line::from(vec![label("title"), Span::raw(t.clone())]));
                    }
                }
            }
        }
        f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if let Some((text, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(6) {
                f.render_widget(Paragraph::new(Line::from(Span::styled(format!(" {text}"), Style::new().fg(Color::Yellow)))), area);
                return;
            }
        }
        let keys = [
            ("x", "kill"),
            ("X", "kill all"),
            ("i", "end idle"),
            ("p", "protect"),
            ("c", "copy"),
            ("o", "open folder"),
            ("r", "refresh"),
            ("s", "settings"),
            ("?", "help"),
            ("q", "quit"),
        ];
        let mut spans = vec![Span::raw(" ")];
        for (k, what) in keys {
            spans.push(Span::styled(k, Style::new().fg(ACCENT).bold()));
            spans.push(Span::styled(format!(" {what}  "), Style::new().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_settings(&self, f: &mut Frame, field: usize, editing: Option<&str>) {
        let area = centered(f.area(), 70, 14);
        f.render_widget(Clear, area);
        let mark = |i: usize| if i == field { Span::styled(" ▸ ", Style::new().fg(ACCENT)) } else { Span::raw("   ") };
        let value = |s: String| Span::styled(s, Style::new().bold());
        let protected = match editing {
            Some(text) => Span::styled(format!("{text}▏"), Style::new().fg(Color::Yellow)),
            None if self.cfg.protected_patterns.is_empty() => Span::styled("(none)", Style::new().fg(DIM)),
            None => value(self.cfg.protected_patterns.join(", ")),
        };
        let lines = vec![
            Line::default(),
            Line::from(vec![mark(0), Span::raw("Scan every            "), value(format!("◂ {} s ▸", self.cfg.refresh_interval))]),
            Line::from(vec![mark(1), Span::raw("Idle after            "), value(format!("◂ {} ▸", format_limit(self.cfg.idle_limit_minutes)))]),
            Line::from(vec![
                mark(2),
                Span::raw("End idle sessions     "),
                value(if self.cfg.ends_idle_sessions { "[x] automatically".into() } else { "[ ] only when asked".into() }),
            ]),
            Line::from(vec![mark(3), Span::raw("Never kill            "), protected]),
            Line::default(),
            Line::from(Span::styled("   Commands containing a protected pattern are skipped by kill session,", Style::new().fg(DIM))),
            Line::from(Span::styled("   kill all and end idle. Patterns are comma-separated.", Style::new().fg(DIM))),
            Line::default(),
            Line::from(Span::styled(
                if editing.is_some() { "   enter save · esc cancel" } else { "   ↑↓ choose · ←→ change · space toggle · enter edit · esc close" },
                Style::new().fg(DIM),
            )),
            Line::from(Span::styled(format!("   {}", Config::path().display()), Style::new().fg(DIM))),
        ];
        let block = Block::bordered().title(" Settings ").border_style(Style::new().fg(ACCENT));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    // MARK: keys

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.on_normal_key(key),
            Mode::Help => {}
            Mode::Confirm { question, targets } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.kill(targets),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.mode = Mode::Confirm { question, targets },
            },
            Mode::Settings { field, editing } => self.on_settings_key(key, field, editing),
        }
    }

    fn selected_kind(&self) -> Option<RowKind> {
        let rows = self.rows(200);
        let key = self.selected_key.as_ref()?;
        rows.into_iter().find(|r| &r.key == key).map(|r| r.kind)
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows(200);
        let Some(current) = self.state.selected() else { return };
        let mut i = current as isize;
        let mut moved = 0;
        loop {
            i += delta.signum();
            if i < 0 || i >= rows.len() as isize {
                break;
            }
            if Self::selectable(&rows[i as usize].kind) {
                self.state.select(Some(i as usize));
                self.selected_key = Some(rows[i as usize].key.clone());
                moved += 1;
                if moved >= delta.unsigned_abs() {
                    break;
                }
            }
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(-10_000),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(10_000),
            KeyCode::Char('r') | KeyCode::F(5) => self.refresh(),
            KeyCode::Char('?') | KeyCode::Char('h') => self.mode = Mode::Help,
            KeyCode::Char('s') => self.mode = Mode::Settings { field: 0, editing: None },
            KeyCode::Char('x') | KeyCode::Delete => self.kill_selected(),
            KeyCode::Char('X') => self.ask_kill_all(),
            KeyCode::Char('i') => self.ask_end_idle(),
            KeyCode::Char('p') => self.toggle_protect(),
            KeyCode::Char('c') => self.copy_selected(),
            KeyCode::Char('o') => self.open_selected_folder(),
            _ => {}
        }
    }

    fn kill_selected(&mut self) {
        let (Some(kind), Some(r)) = (self.selected_kind(), self.result.clone()) else { return };
        match kind {
            RowKind::Orphan(g) => {
                let targets = self.unprotected(&r.orphans[g].procs);
                if targets.is_empty() {
                    self.say("Everything in this session is protected");
                }
                self.kill(targets);
            }
            RowKind::Live(g) => {
                let targets = self.unprotected(&r.live[g].procs);
                self.kill(targets);
            }
            RowKind::OrphanProc(g, p) => self.kill(vec![Target::from(&r.orphans[g].procs[p])]),
            RowKind::LiveProc(g, p) => self.kill(vec![Target::from(&r.live[g].procs[p])]),
            RowKind::SessionProc(s, p) => self.kill(vec![Target::from(&r.sessions[s].procs[p])]),
            RowKind::Session(s) => {
                let session = &r.sessions[s];
                let name = session.title.clone().unwrap_or_else(|| session.project.clone());
                let question = format!(
                    "End the session \"{name}\"{}?\n\nThe conversation is not lost: `claude --resume` brings it back.",
                    if session.status == Status::Busy { ", which is working right now" } else { "" }
                );
                let targets = self.session_targets(std::slice::from_ref(session));
                self.mode = Mode::Confirm { question, targets };
            }
            RowKind::Section | RowKind::Note => {}
        }
    }

    fn ask_kill_all(&mut self) {
        let Some(r) = &self.result else { return };
        let procs = r.killable_orphans(&self.cfg);
        if procs.is_empty() {
            self.say("Nothing left behind to kill");
            return;
        }
        let memory: u64 = procs.iter().map(|p| p.memory).sum();
        let question = format!("Kill {} left behind, holding {}?", plural(procs.len(), "process"), format_bytes(memory));
        self.mode = Mode::Confirm { question, targets: procs.iter().map(Target::from).collect() };
    }

    fn ask_end_idle(&mut self) {
        let Some(r) = &self.result else { return };
        let idle = r.idle_sessions(&self.cfg);
        if idle.is_empty() {
            let limit = format_limit(self.cfg.idle_limit_minutes);
            self.say(format!("No session has been idle longer than {limit} (change it in settings: s)"));
            return;
        }
        let names: Vec<String> = idle.iter().map(|s| format!("  • {}", s.title.clone().unwrap_or_else(|| s.project.clone()))).collect();
        let question = format!(
            "End {} idle longer than {}?\n\n{}\n\nConversations are not lost: `claude --resume` brings them back.",
            plural(idle.len(), "session"),
            format_limit(self.cfg.idle_limit_minutes),
            names.join("\n")
        );
        let targets = self.session_targets(&idle);
        self.mode = Mode::Confirm { question, targets };
    }

    fn toggle_protect(&mut self) {
        let Some(kind) = self.selected_kind() else { return };
        let Some(proc) = self.selected_proc(&kind).cloned() else {
            self.say("Select a process to protect it");
            return;
        };
        if self.cfg.is_protected(&proc.command) {
            self.cfg.unprotect(&proc);
            self.say(format!("{} is no longer protected", proc.executable()));
        } else {
            self.cfg.protect(&proc);
            self.say(format!("{} is protected: kill all and kill session skip it", proc.executable()));
        }
        self.save();
    }

    fn copy_selected(&mut self) {
        let Some(kind) = self.selected_kind() else { return };
        let text = match (self.selected_proc(&kind), &self.result) {
            (Some(p), _) => p.command.clone(),
            (None, Some(r)) => match kind {
                RowKind::Orphan(g) => r.orphans[g].id.clone(),
                RowKind::Live(g) => r.live[g].id.clone(),
                _ => return,
            },
            _ => return,
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self.say("Copied"),
            Err(e) => self.say(format!("Could not copy: {e}")),
        }
    }

    fn open_selected_folder(&mut self) {
        let Some(kind) = self.selected_kind() else { return };
        let Some(cwd) = self.selected_proc(&kind).map(|p| p.cwd.clone()).filter(|c| !c.is_empty()) else {
            self.say("No folder known for this row");
            return;
        };
        if Command::new("explorer.exe").arg(&cwd).spawn().is_err() {
            self.say("Could not open Explorer");
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent, mut field: usize, editing: Option<String>) {
        if let Some(mut text) = editing {
            match key.code {
                KeyCode::Enter => {
                    self.cfg.protected_patterns = text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    self.save();
                    self.mode = Mode::Settings { field, editing: None };
                }
                KeyCode::Esc => self.mode = Mode::Settings { field, editing: None },
                KeyCode::Backspace => {
                    text.pop();
                    self.mode = Mode::Settings { field, editing: Some(text) };
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.mode = Mode::Settings { field, editing: Some(text) };
                }
                _ => self.mode = Mode::Settings { field, editing: Some(text) },
            }
            return;
        }
        let step = |v: u64, steps: &[u64], up: bool| -> u64 {
            if up {
                steps.iter().copied().find(|&s| s > v).unwrap_or(v)
            } else {
                steps.iter().rev().copied().find(|&s| s < v).unwrap_or(v)
            }
        };
        let change = |app: &mut App, field: usize, up: bool| match field {
            0 => app.cfg.refresh_interval = step(app.cfg.refresh_interval, &[5, 10, 15, 30, 60, 120, 300, 600], up),
            1 => {
                let next = if up { app.cfg.idle_limit_minutes + 30 } else { app.cfg.idle_limit_minutes.saturating_sub(30) };
                app.cfg.idle_limit_minutes = next.clamp(MINIMUM_IDLE_LIMIT, 7 * 24 * 60);
            }
            2 => app.cfg.ends_idle_sessions = !app.cfg.ends_idle_sessions,
            _ => {}
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => return,
            KeyCode::Up | KeyCode::Char('k') => field = field.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => field = (field + 1).min(3),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
                change(self, field, false);
                self.after_settings_change(field);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') => {
                change(self, field, true);
                self.after_settings_change(field);
            }
            KeyCode::Char(' ') | KeyCode::Enter if field == 2 => {
                change(self, field, true);
                self.after_settings_change(field);
            }
            KeyCode::Enter if field == 3 => {
                self.mode = Mode::Settings { field, editing: Some(self.cfg.protected_patterns.join(", ")) };
                return;
            }
            _ => {}
        }
        self.mode = Mode::Settings { field, editing: None };
    }

    fn after_settings_change(&mut self, field: usize) {
        self.save();
        if field == 2 && self.cfg.ends_idle_sessions {
            self.refresh();
        }
    }
}

fn kind_prefix(kind: &RowKind) -> &'static str {
    match kind {
        RowKind::Orphan(_) => "og",
        RowKind::OrphanProc(..) => "op",
        RowKind::Session(_) => "s",
        RowKind::SessionProc(..) => "sp",
        RowKind::Live(_) => "lg",
        RowKind::LiveProc(..) => "lp",
        RowKind::Section => "sec",
        RowKind::Note => "note",
    }
}

fn section(title: &str, summary: String) -> Row {
    Row {
        kind: RowKind::Section,
        key: format!("sec:{title}"),
        line: Line::from(vec![
            Span::styled(format!(" {title}"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(if summary.is_empty() { String::new() } else { format!("  {summary}") }, Style::new().fg(DIM)),
        ]),
    }
}

/// Lays out a row: `left` truncated to fit, `right` against the right edge.
fn fit(left: Vec<Span<'static>>, right: Span<'static>, width: usize) -> Line<'static> {
    let right_len = right.content.chars().count();
    let room = width.saturating_sub(right_len);
    let mut used = 0;
    let mut spans = Vec::new();
    for span in left {
        let len = span.content.chars().count();
        if used + len <= room {
            used += len;
            spans.push(span);
        } else {
            let keep = room.saturating_sub(used + 1);
            if keep > 0 {
                let text: String = span.content.chars().take(keep).collect();
                spans.push(Span::styled(format!("{text}…"), span.style));
                used += keep + 1;
            }
            break;
        }
    }
    spans.push(Span::raw(" ".repeat(room.saturating_sub(used))));
    spans.push(right);
    Line::from(spans)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_confirm(f: &mut Frame, question: &str) {
    let lines = question.lines().count() as u16;
    let area = centered(f.area(), 72, lines + 6);
    f.render_widget(Clear, area);
    let mut text = Text::default();
    text.lines.push(Line::default());
    for line in question.lines() {
        text.lines.push(Line::from(format!(" {line}")));
    }
    text.lines.push(Line::default());
    text.lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(" y ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw(" yes   "),
        Span::styled(" n ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" no"),
    ]));
    let block = Block::bordered().title(" Confirm ").border_style(Style::new().fg(ACCENT));
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 78, 26);
    f.render_widget(Clear, area);
    let key = |k: &str, what: &str| Line::from(vec![Span::styled(format!("  {k:<12}"), Style::new().fg(ACCENT).bold()), Span::raw(what.to_string())]);
    let dim = |s: &str| Line::from(Span::styled(format!("  {s}"), Style::new().fg(DIM)));
    let lines = vec![
        Line::default(),
        key("↑ ↓  j k", "move"),
        key("x  Del", "kill the selected process, or everything in the selected session"),
        key("", "on an open session: end it (asks first)"),
        key("X", "kill everything left behind (asks first)"),
        key("i", "end sessions idle longer than the limit (asks first)"),
        key("p", "protect / unprotect the selected program"),
        key("c", "copy the command (or session ID)"),
        key("o", "open the process's folder in Explorer"),
        key("r  F5", "scan now"),
        key("s", "settings"),
        key("q  Esc", "quit"),
        Line::default(),
        dim("Claude Code sets CLAUDECODE=1, the session ID and its own PID in the"),
        dim("environment of everything it starts. seshka reads those from your processes;"),
        dim("a process is left behind once the Claude Code process that started it is gone."),
        dim("Whatever a marked process started counts too, so a browser opened by"),
        dim("Playwright shows next to the process that opened it. An app's helper"),
        dim("processes are folded into one row and go when it is killed."),
        Line::default(),
        dim("Windows does not end children with their parent, so killing a row also ends"),
        dim("what it started. Windowed apps are asked to close first; the rest are ended."),
        dim("Nothing leaves your machine."),
    ];
    let block = Block::bordered().title(" seshka · any key to close ").border_style(Style::new().fg(ACCENT));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    // clip.exe reads UTF-16 when the input starts with a byte-order mark.
    let mut child = Command::new("clip.exe").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    child.stdin.take().expect("piped stdin").write_all(&bytes)?;
    child.wait()?;
    Ok(())
}
