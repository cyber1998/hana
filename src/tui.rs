//! The interactive view: what the macOS menu bar popover shows, in a terminal, plus a storage tab
//! for session data and worktrees. Two looks: classic and kawaii.

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
use unicode_width::UnicodeWidthChar;

use crate::disk::{self, Outcome, SessionData, StorageResult, Worktree};
use crate::scanner::{self, Proc, RunningSession, SessionGroup, Snapshot, Status};
use crate::themes::{self, Theme};
use crate::store::{self, format_bytes, format_limit, human_age, plural, Config, KillReport, ScanResult, Target, MINIMUM_IDLE_LIMIT};

// MARK: state

enum Msg {
    Scanned(ScanResult),
    Killed { targets: Vec<u32>, report: KillReport },
    Storage { generation: u64, result: StorageResult },
    Measured { generation: u64, path: String, measured: disk::Measured },
    MeasuringDone { generation: u64 },
    Deleted(Outcome),
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Processes,
    Storage,
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
    Worktree(usize),
    SessionData(usize),
}

struct Row {
    kind: RowKind,
    /// Identifies the row across scans, so the selection stays put when the list changes.
    key: String,
    line: Line<'static>,
}

/// What a confirmation leads to.
enum Pending {
    Kill(Vec<Target>),
    /// Session data to recycle and worktrees to remove. Risky worktrees go only with force.
    Delete { sessions: Vec<SessionData>, worktrees: Vec<Worktree>, risky: usize },
}

enum Mode {
    Normal,
    Confirm { question: String, action: Pending },
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
    /// What the most recent kills or deletes released; ones in quick succession add up into one message.
    freed: Option<(u64, String, Instant)>,
    last_auto_end: Option<(usize, SystemTime)>,
    message: Option<(String, Instant)>,
    state: ListState,
    selected_key: Option<String>,
    mode: Mode,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
    view: View,
    /// The selection of the view not shown, kept for switching back.
    other_selection: (ListState, Option<String>),
    storage: Option<StorageResult>,
    storage_scanning: bool,
    /// Bumped by every storage scan, so results of an older one are dropped.
    storage_generation: u64,
    measuring: bool,
    deleting: bool,
    /// Storage rows picked with space, by key, to delete together.
    marked: HashSet<String>,
}

pub fn run(cfg: &mut Config, storage_first: bool) -> io::Result<()> {
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
        view: View::Processes,
        other_selection: (ListState::default(), None),
        storage: None,
        storage_scanning: false,
        storage_generation: 0,
        measuring: false,
        deleting: false,
        marked: HashSet::new(),
    };
    app.refresh();
    if storage_first {
        app.switch_view(View::Storage);
    }
    let outcome = app.event_loop(&mut terminal);
    ratatui::restore();
    *cfg = app.cfg;
    outcome
}

impl App {
    fn theme(&self) -> &'static Theme {
        themes::by_name(&self.cfg.theme)
    }

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

    /// Session data and worktrees. Worktree sizes arrive one by one afterwards; big ones take a while.
    fn refresh_storage(&mut self) {
        if self.storage_scanning {
            return;
        }
        self.storage_scanning = true;
        self.measuring = true;
        self.storage_generation += 1;
        let generation = self.storage_generation;
        let (open, cwds) = self.open_sessions();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = disk::scan(&open, &cwds);
            let worktrees = result.worktrees.clone();
            let _ = tx.send(Msg::Storage { generation, result });
            thread::scope(|scope| {
                for wt in &worktrees {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let measured = disk::measure(wt);
                        let _ = tx.send(Msg::Measured { generation, path: wt.path.clone(), measured });
                    });
                }
            });
            let _ = tx.send(Msg::MeasuringDone { generation });
        });
    }

    /// IDs and folders of the sessions open right now.
    fn open_sessions(&self) -> (HashSet<String>, Vec<String>) {
        let sessions = self.result.as_ref().map(|r| r.sessions.as_slice()).unwrap_or_default();
        (
            sessions.iter().filter_map(|s| s.session_id.clone()).collect(),
            sessions.iter().map(|s| s.process.cwd.clone()).filter(|c| !c.is_empty()).collect(),
        )
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Scanned(result) => {
                self.result = Some(result);
                self.scanning = false;
                self.last_scan = Some(Instant::now());
                // Keep "open" marks in the storage tab current.
                let (open, _) = self.open_sessions();
                if let Some(storage) = &mut self.storage {
                    for s in &mut storage.sessions {
                        s.open = open.contains(&s.id);
                    }
                }
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
                    let what = plural(report.all_gone.len(), "process");
                    self.announce_freed(report.freed, format!("{what} {}", self.theme().killed_verb));
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
            Msg::Storage { generation, mut result } if generation == self.storage_generation => {
                // The process scan may have finished after this one started.
                if self.result.is_some() {
                    let (open, _) = self.open_sessions();
                    for s in &mut result.sessions {
                        s.open = open.contains(&s.id);
                    }
                }
                self.storage = Some(result);
                self.storage_scanning = false;
            }
            Msg::Measured { generation, path, measured } if generation == self.storage_generation => {
                if let Some(wt) = self.storage.as_mut().and_then(|s| s.worktrees.iter_mut().find(|w| w.path == path)) {
                    wt.bytes = Some(measured.bytes);
                    wt.dirty = measured.dirty;
                    wt.only_here = measured.only_here;
                }
            }
            Msg::MeasuringDone { generation } if generation == self.storage_generation => {
                self.measuring = false;
                if let Some(s) = &mut self.storage {
                    s.worktrees.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
                }
            }
            Msg::Deleted(outcome) => {
                self.deleting = false;
                let mut done = Vec::new();
                if outcome.sessions_deleted > 0 {
                    done.push(format!("{} recycled", plural(outcome.sessions_deleted, "session")));
                }
                if outcome.worktrees_removed > 0 {
                    done.push(format!("{} removed", plural(outcome.worktrees_removed, "worktree")));
                }
                if !done.is_empty() {
                    self.announce_freed(outcome.freed, done.join(", "));
                }
                if !outcome.problems.is_empty() {
                    let more = if outcome.problems.len() > 2 { format!(" (+{} more)", outcome.problems.len() - 2) } else { String::new() };
                    self.say(format!("{}{more}", outcome.problems.iter().take(2).cloned().collect::<Vec<_>>().join(" · ")));
                }
                self.marked.clear();
                self.storage_scanning = false;
                self.refresh_storage();
                self.refresh();
            }
            // Results of a superseded storage scan.
            _ => {}
        }
    }

    fn announce_freed(&mut self, bytes: u64, what: String) {
        let (bytes, what) = match &self.freed {
            Some((b, w, t)) if t.elapsed() < Duration::from_secs(15) => (b + bytes, format!("{w}, {what}")),
            _ => (bytes, what),
        };
        self.freed = Some((bytes, what, Instant::now()));
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

    fn delete(&mut self, sessions: Vec<SessionData>, worktrees: Vec<Worktree>, force: bool) {
        if self.deleting {
            return;
        }
        self.deleting = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            // Decide what is open from a fresh scan, not from what was on screen.
            let fresh = store::full_scan();
            let open: HashSet<String> = fresh.sessions.iter().filter_map(|s| s.session_id.clone()).collect();
            let snap = Snapshot::take();
            let mut outcome = Outcome::default();
            disk::delete_sessions(&sessions, &open, &mut outcome);
            for wt in &worktrees {
                disk::remove_worktree(wt, force, &snap, &mut outcome);
            }
            let _ = tx.send(Msg::Deleted(outcome));
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

    fn switch_view(&mut self, to: View) {
        if self.view == to {
            return;
        }
        self.view = to;
        let current = (std::mem::take(&mut self.state), self.selected_key.take());
        let (state, key) = std::mem::replace(&mut self.other_selection, current);
        self.state = state;
        self.selected_key = key;
        if to == View::Storage && self.storage.is_none() {
            self.refresh_storage();
        }
    }

    // MARK: rows

    fn rows(&self, width: u16) -> Vec<Row> {
        match self.view {
            View::Processes => self.process_rows(width as usize),
            View::Storage => self.storage_rows(width as usize),
        }
    }

    fn note(&self, key: &str, text: &str, color: Color) -> Row {
        Row { kind: RowKind::Note, key: key.into(), line: Line::from(Span::styled(text.to_string(), Style::new().fg(color))) }
    }

    fn gap(key: &str) -> Row {
        Row { kind: RowKind::Note, key: key.into(), line: Line::default() }
    }

    fn section(&self, title: &str, summary: String) -> Row {
        let t = self.theme();
        let title = if t.lowercase_sections { title.to_lowercase() } else { title.to_string() };
        Row {
            kind: RowKind::Section,
            key: format!("sec:{title}"),
            line: Line::from(vec![
                Span::styled(format!("{}{title}{}", t.section_left, t.section_right), Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::styled(if summary.is_empty() { String::new() } else { format!("  {summary}") }, Style::new().fg(t.dim)),
            ]),
        }
    }

    fn process_rows(&self, width: usize) -> Vec<Row> {
        let t = self.theme();
        let mut rows = Vec::new();
        let Some(r) = &self.result else {
            rows.push(self.note("scanning", "  Looking…", t.dim));
            return rows;
        };
        let now = scanner::now_ms();

        if r.orphans.is_empty() {
            rows.push(self.section("LEFT BEHIND", String::new()));
            rows.push(self.note("clean", t.clean, t.good));
        } else {
            rows.push(self.section(
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

        rows.push(Self::gap("gap1"));
        let idle_count = r.idle_sessions(&self.cfg).len();
        let summary = if r.sessions.is_empty() {
            String::new()
        } else if idle_count > 0 {
            format!("{} · {} idle over {}", r.sessions.len(), idle_count, format_limit(self.cfg.idle_limit_minutes))
        } else {
            r.sessions.len().to_string()
        };
        rows.push(self.section("OPEN SESSIONS", summary));
        if r.sessions.is_empty() {
            rows.push(self.note("nosessions", t.no_sessions, t.dim));
        }
        for (s, session) in r.sessions.iter().enumerate() {
            rows.push(self.session_row(s, session, now, width));
            for (p, proc) in session.procs.iter().enumerate() {
                rows.push(self.proc_row(RowKind::SessionProc(s, p), proc, width));
            }
        }

        if !r.live.is_empty() {
            rows.push(Self::gap("gap2"));
            rows.push(self.section("RUNNING IN OTHER SESSIONS", String::new()));
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
        let t = self.theme();
        let mut left = vec![Span::styled(format!(" {}", group.project), Style::new().fg(t.text).bold())];
        if let Some(title) = &group.title {
            left.push(Span::styled(format!(" — {title}"), Style::new().fg(t.text)));
        }
        left.push(Span::styled(format!("  {}", group.short_id()), Style::new().fg(t.dim)));
        let busy = group.procs.iter().any(|p| self.killing.contains(&p.pid));
        let right = if busy {
            Span::styled(format!("{:>28} ", t.killing), Style::new().fg(t.accent))
        } else {
            Span::raw(format!("{:>12}{:>10}{:>6} ", plural(group.procs.len(), "proc"), format_bytes(group.memory()), human_age(group.oldest())))
        };
        Row { key: format!("{}:{}", kind_prefix(&kind), group.id), kind, line: fit(left, right, width) }
    }

    fn proc_row(&self, kind: RowKind, proc: &Proc, width: usize) -> Row {
        let t = self.theme();
        let protected = self.cfg.is_protected(&proc.command);
        let ports = proc.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ");
        let killing = self.killing.contains(&proc.pid);
        let right = if killing {
            Span::styled(format!("{:>28} ", t.killing), Style::new().fg(t.accent))
        } else {
            Span::styled(format!("{:>12}{:>10}{:>6} ", ports, format_bytes(proc.memory), human_age(proc.age)), Style::new().fg(t.text))
        };
        let mut left = vec![Span::raw("     "), Span::styled(proc.short_command(), Style::new().fg(if protected { t.dim } else { t.text }))];
        if protected {
            left.push(Span::styled("  protected", Style::new().fg(t.info)));
        }
        if !proc.helpers.is_empty() {
            left.push(Span::styled(format!("  +{}", proc.helpers.len()), Style::new().fg(t.dim)));
        }
        Row { key: format!("{}:{}:{}", kind_prefix(&kind), proc.pid, proc.created), kind, line: fit(left, right, width) }
    }

    fn session_row(&self, s: usize, session: &RunningSession, now: f64, width: usize) -> Row {
        let t = self.theme();
        let limit = self.cfg.idle_limit_minutes * 60;
        let (dot, status) = match (&session.status, session.idle_seconds(now)) {
            (Status::Idle, Some(idle)) if idle >= limit => (Span::styled(t.idle_dot, Style::new().fg(t.accent)), format!("idle {}", human_age(idle))),
            (Status::Idle, Some(idle)) => (Span::styled(t.idle_dot, Style::new().fg(t.warn)), format!("idle {}", human_age(idle))),
            (Status::Idle, None) => (Span::styled(t.idle_dot, Style::new().fg(t.warn)), "idle".into()),
            (Status::Busy, _) => (Span::styled(t.busy_dot, Style::new().fg(t.good)), "working".into()),
            (Status::Other(o), _) => (Span::styled(t.unknown_dot, Style::new().fg(t.dim)), o.clone()),
            (Status::Unknown, _) => (Span::styled(t.unknown_dot, Style::new().fg(t.dim)), "open".into()),
        };
        let mut left = vec![
            dot,
            Span::styled(format!(" {status:<9} "), Style::new().fg(t.dim)),
            Span::styled(session.project.clone(), Style::new().fg(t.text).bold()),
        ];
        if let Some(title) = &session.title {
            left.push(Span::styled(format!(" — {title}"), Style::new().fg(t.text)));
        }
        let right = if self.killing.contains(&session.process.pid) {
            Span::styled(format!("{:>28} ", t.ending), Style::new().fg(t.accent))
        } else {
            Span::styled(
                format!("{:>12}{:>10}{:>6} ", format!("pid {}", session.process.pid), format_bytes(session.memory()), human_age(session.process.age)),
                Style::new().fg(t.text),
            )
        };
        Row { key: format!("s:{}:{}", session.process.pid, session.process.created), kind: RowKind::Session(s), line: fit(left, right, width) }
    }

    fn mark_span(&self, key: &str) -> Span<'static> {
        let t = self.theme();
        if self.marked.contains(key) {
            Span::styled(format!(" {}", t.marked), Style::new().fg(t.accent))
        } else {
            Span::styled(format!(" {}", t.unmarked), Style::new().fg(t.dim))
        }
    }

    fn storage_rows(&self, width: usize) -> Vec<Row> {
        let t = self.theme();
        let mut rows = Vec::new();
        let Some(s) = &self.storage else {
            rows.push(self.note("scanning", "  Looking through ~/.claude and your repositories…", t.dim));
            return rows;
        };
        let (_, open_cwds) = self.open_sessions();

        let measured = s.worktrees.iter().filter(|w| w.bytes.is_some()).count();
        let mut summary = format!("{} · {}", s.worktrees.len(), format_bytes(s.worktree_bytes()));
        if self.measuring {
            summary.push_str(&format!(" · {} {measured}/{}", t.measuring, s.worktrees.len()));
        }
        rows.push(self.section("WORKTREES", summary));
        if s.worktrees.is_empty() {
            rows.push(self.note("nowt", "  No worktrees in the repositories your sessions worked in.", t.dim));
        }
        for (i, wt) in s.worktrees.iter().enumerate() {
            let key = format!("wt:{}", wt.path);
            let mut left = vec![
                self.mark_span(&key),
                Span::styled(wt.repo_name().to_string(), Style::new().fg(t.dim)),
                Span::styled(" ⑂ ", Style::new().fg(t.accent)),
                Span::styled(wt.name().to_string(), Style::new().fg(t.text).bold()),
            ];
            let branch = match &wt.branch {
                Some(b) => format!("  {b}"),
                None if wt.registered => "  detached".into(),
                None => String::new(),
            };
            left.push(Span::styled(branch, Style::new().fg(t.info)));
            let in_use = open_cwds.iter().any(|c| inside(c, &wt.path));
            for (show, text, color) in [
                (in_use, " open session".to_string(), t.good),
                (wt.dirty.unwrap_or(0) > 0, format!(" {} uncommitted", wt.dirty.unwrap_or(0)), t.warn),
                (wt.only_here.unwrap_or(0) > 0, format!(" {} commits on no branch", wt.only_here.unwrap_or(0)), t.warn),
                (wt.locked, " locked".into(), t.warn),
                (!wt.exists, " folder gone".into(), t.dim),
                (!wt.registered, " unknown to git".into(), t.dim),
            ] {
                if show {
                    left.push(Span::styled(format!(" ·{text}"), Style::new().fg(color)));
                }
            }
            let size = match wt.bytes {
                _ if self.deleting && self.marked.contains(&key) => t.deleting.to_string(),
                Some(b) => format_bytes(b),
                None => "…".into(),
            };
            let right = Span::styled(format!("{:>14}{:>10} ", plural(wt.sessions.len(), "session"), size), Style::new().fg(t.text));
            rows.push(Row { kind: RowKind::Worktree(i), key, line: fit(left, right, width) });
        }

        rows.push(Self::gap("gap3"));
        let old = s.sessions.iter().filter(|d| !d.open && d.age.unwrap_or(0) >= 30 * 86400).count();
        rows.push(self.section(
            "SESSION DATA",
            format!("{} · {} · {old} untouched for 30+ days", plural(s.sessions.len(), "session"), format_bytes(s.session_bytes())),
        ));
        if s.sessions.is_empty() {
            rows.push(self.note("nodata", t.no_data, t.dim));
        }
        for (i, data) in s.sessions.iter().enumerate() {
            let key = format!("sd:{}", data.id);
            let mut left = vec![self.mark_span(&key)];
            if data.open {
                left.push(Span::styled(format!("{} ", t.busy_dot.trim()), Style::new().fg(t.good)));
            }
            left.push(Span::styled(data.project.clone(), Style::new().fg(t.text).bold()));
            if let Some(title) = &data.title {
                left.push(Span::styled(format!(" — {title}"), Style::new().fg(t.text)));
            }
            left.push(Span::styled(format!("  {}", data.short_id()), Style::new().fg(t.dim)));
            let right = Span::styled(
                format!("{:>10}{:>6} ", format_bytes(data.bytes), data.age.map(human_age).unwrap_or_default()),
                Style::new().fg(if data.open { t.dim } else { t.text }),
            );
            rows.push(Row { kind: RowKind::SessionData(i), key, line: fit(left, right, width) });
        }
        rows
    }

    fn selectable(kind: &RowKind) -> bool {
        !matches!(kind, RowKind::Section | RowKind::Note)
    }

    // MARK: drawing

    fn draw(&mut self, f: &mut Frame) {
        let t = self.theme();
        if t.bg.is_some() {
            f.render_widget(Block::new().style(t.base()), f.area());
        }
        let details_height = if self.view == View::Storage { 7 } else { 5 };
        let [header, body, details, footer] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(4), Constraint::Length(details_height), Constraint::Length(1)]).areas(f.area());

        // One column of slack: terminals differ on how wide some symbols are, and a row that reaches
        // the last column can wrap.
        let rows = self.rows(body.width.saturating_sub(1));
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
        let list = List::new(items).highlight_style(Style::new().bg(t.select_bg));
        f.render_stateful_widget(list, body, &mut self.state);
        let selected = index.map(|i| rows[i].kind.clone());
        self.draw_details(f, details, selected.as_ref());
        self.draw_footer(f, footer);

        match &self.mode {
            Mode::Normal => {}
            Mode::Confirm { question, action } => {
                let risky = matches!(action, Pending::Delete { risky, .. } if *risky > 0);
                self.draw_confirm(f, question, risky)
            }
            Mode::Settings { field, editing } => self.draw_settings(f, *field, editing.as_deref()),
            Mode::Help => self.draw_help(f),
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let t = self.theme();
        let logo = match &self.result {
            Some(r) if r.orphan_count() > 0 => Span::styled(t.logo_dirty, Style::new().fg(t.accent).bold()),
            Some(_) => Span::styled(t.logo_clean, Style::new().fg(t.good).bold()),
            None => Span::styled(t.logo_loading, Style::new().fg(t.text).bold()),
        };
        let tab = |label: &str, view: View| {
            if self.view == view {
                Span::styled(format!(" {label} "), Style::new().fg(t.on_accent).bg(t.accent).bold())
            } else {
                Span::styled(format!(" {label} "), Style::new().fg(t.dim))
            }
        };
        let mut spans = vec![logo, Span::raw("  "), tab("1 processes", View::Processes), Span::raw(" "), tab("2 storage", View::Storage)];
        spans.push(Span::styled(t.tagline, Style::new().fg(t.dim)));
        let status = if self.view == View::Storage {
            if self.storage_scanning {
                "looking…".to_string()
            } else if self.deleting {
                t.deleting.to_string()
            } else {
                "r to rescan".to_string()
            }
        } else if self.scanning {
            "scanning…".to_string()
        } else if let Some(at) = self.last_scan {
            format!("scanned {} ago · every {}s", human_age(at.elapsed().as_secs()), self.cfg.refresh_interval)
        } else {
            String::new()
        };
        let used: usize = spans.iter().map(Span::width).sum();
        let status_width = Span::raw(status.as_str()).width();
        if used + status_width + 2 < area.width as usize {
            spans.push(Span::raw(" ".repeat(area.width as usize - used - status_width - 2)));
            spans.push(Span::styled(status, Style::new().fg(t.dim)));
        }
        let mut lines = vec![Line::from(spans)];

        let mut second = Vec::new();
        if let Some((bytes, what, at)) = &self.freed {
            if at.elapsed() < Duration::from_secs(15) {
                second.push(Span::styled(format!("{}{}", t.freed, format_bytes(*bytes)), Style::new().fg(t.good).bold()));
                second.push(Span::styled(format!(" · {what}{}", t.freed_suffix), Style::new().fg(t.text)));
            }
        }
        if second.is_empty() {
            if let Some((count, at)) = self.last_auto_end {
                let ago = at.elapsed().map(|d| human_age(d.as_secs())).unwrap_or_default();
                second.push(Span::styled(format!(" Ended {} idle automatically, {ago} ago", plural(count, "session")), Style::new().fg(t.dim)));
            }
        }
        if second.is_empty() && self.view == View::Storage && !self.marked.is_empty() {
            let bytes = self.marked_bytes();
            second.push(Span::styled(
                format!(" {} marked · {} · x to delete, u to unmark", self.marked.len(), format_bytes(bytes)),
                Style::new().fg(t.accent),
            ));
        }
        lines.push(Line::from(second));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn marked_bytes(&self) -> u64 {
        let Some(s) = &self.storage else { return 0 };
        let sessions: u64 = s.sessions.iter().filter(|d| self.marked.contains(&format!("sd:{}", d.id))).map(|d| d.bytes).sum();
        let worktrees: u64 = s.worktrees.iter().filter(|w| self.marked.contains(&format!("wt:{}", w.path))).filter_map(|w| w.bytes).sum();
        sessions + worktrees
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
        let t = self.theme();
        let block = Block::new().borders(Borders::TOP).border_type(t.border).border_style(Style::new().fg(t.dim));
        let mut lines: Vec<Line> = Vec::new();
        let label = |s: &str| Span::styled(format!("{s:<10}"), Style::new().fg(t.dim));
        let value = |s: String| Span::styled(s, Style::new().fg(t.text));
        match kind {
            Some(RowKind::Worktree(i)) => {
                if let Some(wt) = self.storage.as_ref().and_then(|s| s.worktrees.get(*i)) {
                    lines.push(Line::from(vec![label("worktree"), value(wt.path.clone())]));
                    let head = if wt.head.is_empty() { String::new() } else { format!(" at {}", &wt.head[..wt.head.len().min(10)]) };
                    let branch = wt.branch.clone().map(|b| format!("branch {b} (kept when removed)")).unwrap_or_else(|| "detached HEAD".into());
                    lines.push(Line::from(vec![label("checkout"), value(format!("{branch}{head}"))]));
                    let status = match (wt.dirty, wt.only_here) {
                        (Some(0), Some(0)) => "clean: safe to remove".to_string(),
                        (Some(d), o) => format!("{d} uncommitted · {} commits on no branch", o.unwrap_or(0)),
                        _ if !wt.exists => "the folder is gone; removing prunes it from git".into(),
                        _ if !wt.registered => "a leftover folder git does not know about".into(),
                        _ => "checking…".into(),
                    };
                    lines.push(Line::from(vec![label("status"), value(status)]));
                    let titles: Vec<String> = self
                        .storage
                        .as_ref()
                        .map(|s| {
                            wt.sessions
                                .iter()
                                .filter_map(|id| s.sessions.iter().find(|d| &d.id == id))
                                .map(|d| d.title.clone().unwrap_or_else(|| d.short_id().to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !titles.is_empty() {
                        lines.push(Line::from(vec![label("sessions"), value(titles.join(" · "))]));
                    }
                }
            }
            Some(RowKind::SessionData(i)) => {
                if let Some(d) = self.storage.as_ref().and_then(|s| s.sessions.get(*i)) {
                    let mut first = vec![label("session"), value(d.id.clone())];
                    if d.open {
                        first.push(Span::styled("  open now: close it to delete its data", Style::new().fg(t.good)));
                    }
                    lines.push(Line::from(first));
                    if let Some(cwd) = &d.cwd {
                        lines.push(Line::from(vec![label("folder"), value(cwd.clone())]));
                    }
                    let mut parts = d.parts.clone();
                    parts.sort_by(|a, b| b.bytes.cmp(&a.bytes));
                    for p in parts.iter().take(4) {
                        lines.push(Line::from(vec![
                            label(p.kind.split(' ').next().unwrap_or(p.kind)),
                            Span::styled(format!("{:>8}  ", format_bytes(p.bytes)), Style::new().fg(t.accent)),
                            Span::styled(p.path.display().to_string(), Style::new().fg(t.dim)),
                        ]));
                    }
                }
            }
            Some(kind) => {
                if let Some(p) = self.selected_proc(kind) {
                    let mut first = vec![label("pid"), value(p.pid.to_string())];
                    if !p.helpers.is_empty() {
                        first.push(Span::styled(
                            format!("  + {} helper{}", p.helpers.len(), if p.helpers.len() == 1 { "" } else { "s" }),
                            Style::new().fg(t.dim),
                        ));
                    }
                    first.push(Span::styled(format!("   started {} ago", human_age(p.age)), Style::new().fg(t.dim)));
                    if !p.ports.is_empty() {
                        first.push(value(format!("   ports {}", p.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "))));
                    }
                    lines.push(Line::from(first));
                    lines.push(Line::from(vec![label("cwd"), value(p.cwd.clone())]));
                    lines.push(Line::from(vec![label("command"), value(p.command.replace(['\n', '\r'], " "))]));
                } else if let Some(r) = &self.result {
                    let group = match *kind {
                        RowKind::Orphan(g) => r.orphans.get(g),
                        RowKind::Live(g) => r.live.get(g),
                        _ => None,
                    };
                    if let Some(g) = group {
                        lines.push(Line::from(vec![label("session"), value(g.id.clone())]));
                        lines.push(Line::from(vec![label("project"), value(g.project.clone())]));
                        if let Some(title) = &g.title {
                            lines.push(Line::from(vec![label("title"), value(title.clone())]));
                        }
                    }
                }
            }
            None => {}
        }
        f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let t = self.theme();
        if let Some((text, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(6) {
                f.render_widget(Paragraph::new(Line::from(Span::styled(format!(" {text}"), Style::new().fg(t.warn)))), area);
                return;
            }
        }
        let keys: &[(&str, &str)] = match self.view {
            View::Processes => &[
                ("x", "kill"),
                ("X", "kill all"),
                ("i", "end idle"),
                ("p", "protect"),
                ("tab", "storage"),
                ("s", "settings"),
                ("t", "theme"),
                ("?", "help"),
                ("q", "quit"),
            ],
            View::Storage => &[
                ("space", "mark"),
                ("x", "delete"),
                ("M", "mark 30d+"),
                ("u", "unmark"),
                ("r", "rescan"),
                ("tab", "processes"),
                ("t", "theme"),
                ("?", "help"),
                ("q", "quit"),
            ],
        };
        let mut spans = vec![Span::raw(" ")];
        for (i, (k, what)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(t.key_sep, Style::new().fg(t.dim)));
            }
            spans.push(Span::styled(*k, Style::new().fg(t.accent).bold()));
            spans.push(Span::styled(format!(" {what}"), Style::new().fg(t.dim)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_confirm(&self, f: &mut Frame, question: &str, risky: bool) {
        let t = self.theme();
        let height = question.lines().count() as u16 + 6;
        let area = centered(f.area(), 80, height);
        f.render_widget(Clear, area);
        let mut text = Text::default();
        text.lines.push(Line::default());
        for line in question.lines() {
            text.lines.push(Line::from(Span::styled(format!(" {line}"), Style::new().fg(t.text))));
        }
        text.lines.push(Line::default());
        let mut buttons = vec![
            Span::raw(" "),
            Span::styled(" y ", Style::new().fg(t.on_accent).bg(t.accent).bold()),
            Span::styled(" yes   ", Style::new().fg(t.text)),
        ];
        if risky {
            buttons.push(Span::styled(" F ", Style::new().fg(t.on_accent).bg(t.warn).bold()));
            buttons.push(Span::styled(" yes, and force the risky ones   ", Style::new().fg(t.text)));
        }
        buttons.push(Span::styled(" n ", Style::new().fg(t.on_accent).bg(t.dim)));
        buttons.push(Span::styled(" no", Style::new().fg(t.text)));
        text.lines.push(Line::from(buttons));
        let block = Block::bordered().border_type(t.border).style(t.base()).title(t.confirm_title).border_style(Style::new().fg(t.accent));
        f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
    }

    fn draw_settings(&self, f: &mut Frame, field: usize, editing: Option<&str>) {
        let t = self.theme();
        let area = centered(f.area(), 72, 15);
        f.render_widget(Clear, area);
        let mark = |i: usize| if i == field { Span::styled(" ▸ ", Style::new().fg(t.accent)) } else { Span::raw("   ") };
        let name = |s: &str| Span::styled(format!("{s:<22}"), Style::new().fg(t.text));
        let value = |s: String| Span::styled(s, Style::new().fg(t.text).bold());
        let hint = |s: String| Line::from(Span::styled(s, Style::new().fg(t.dim)));
        let protected = match editing {
            Some(text) => Span::styled(format!("{text}▏"), Style::new().fg(t.warn)),
            None if self.cfg.protected_patterns.is_empty() => Span::styled("(none)", Style::new().fg(t.dim)),
            None => value(self.cfg.protected_patterns.join(", ")),
        };
        let lines = vec![
            Line::default(),
            Line::from(vec![mark(0), name("Scan every"), value(format!("◂ {} s ▸", self.cfg.refresh_interval))]),
            Line::from(vec![mark(1), name("Idle after"), value(format!("◂ {} ▸", format_limit(self.cfg.idle_limit_minutes)))]),
            Line::from(vec![
                mark(2),
                name("End idle sessions"),
                value(if self.cfg.ends_idle_sessions { "[x] automatically".into() } else { "[ ] only when asked".into() }),
            ]),
            Line::from(vec![mark(3), name("Never kill"), protected]),
            Line::from(vec![mark(4), name("Look"), value(format!("◂ {} ▸", t.name))]),
            Line::default(),
            hint("   Commands containing a protected pattern are skipped by kill session,".into()),
            hint("   kill all and end idle. Patterns are comma-separated.".into()),
            Line::default(),
            hint(
                if editing.is_some() { "   enter save · esc cancel" } else { "   ↑↓ choose · ←→ change · space toggle · enter edit · esc close" }.into(),
            ),
            hint(format!("   {}", Config::path().display())),
        ];
        let block = Block::bordered().border_type(t.border).style(t.base()).title(t.settings_title).border_style(Style::new().fg(t.accent));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let t = self.theme();
        let area = centered(f.area(), 82, 32);
        f.render_widget(Clear, area);
        let key = |k: &str, what: &str| {
            Line::from(vec![Span::styled(format!("  {k:<12}"), Style::new().fg(t.accent).bold()), Span::styled(what.to_string(), Style::new().fg(t.text))])
        };
        let head = |s: &str| Line::from(Span::styled(format!(" {s}"), Style::new().fg(t.info).bold()));
        let dim = |s: &str| Line::from(Span::styled(format!("  {s}"), Style::new().fg(t.dim)));
        let lines = vec![
            head("everywhere"),
            key("↑ ↓  j k", "move · tab / 1 / 2 switch tabs · s settings · q quit"),
            key("t / T", "next / previous theme (11 of them; also in settings)"),
            Line::default(),
            head("processes"),
            key("x  Del", "kill the selected process, or everything in the selected session"),
            key("", "on an open session: end it (asks first)"),
            key("X", "kill everything left behind (asks first)"),
            key("i", "end sessions idle longer than the limit (asks first)"),
            key("p", "protect / unprotect the selected program"),
            key("c / o", "copy the command / open the process's folder"),
            Line::default(),
            head("storage"),
            key("space", "mark a worktree or session; M marks sessions untouched for 30+ days"),
            key("x  Del", "delete what is marked (or the selected row), after asking"),
            key("u / r", "unmark everything / scan again"),
            dim("Session data (transcript, subagents, file history…) goes to the Recycle Bin."),
            dim("Without its transcript a session can no longer be resumed. Open sessions are"),
            dim("never touched, and neither is a project's memory folder."),
            dim("Worktrees are removed with git worktree remove: the folder is deleted for good,"),
            dim("its branch is kept. One with uncommitted changes or commits on no branch is"),
            dim("only removed if you press F; one a process is working in, never."),
            Line::default(),
            dim("How it works: Claude Code marks everything it starts (CLAUDECODE=1, session ID,"),
            dim("its PID). hana reads those; a process is left behind once its Claude Code is"),
            dim("gone. Windows does not end children with their parent, so killing a row also"),
            dim("ends what it started. Nothing leaves your machine."),
        ];
        let block = Block::bordered().border_type(t.border).style(t.base()).title(t.help_title).border_style(Style::new().fg(t.accent));
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
            Mode::Confirm { question, action } => match (key.code, action) {
                (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, action) => self.run(action, false),
                (KeyCode::Char('F'), action @ Pending::Delete { risky: 1.., .. }) => self.run(action, true),
                (KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q'), _) => {}
                (_, action) => self.mode = Mode::Confirm { question, action },
            },
            Mode::Settings { field, editing } => self.on_settings_key(key, field, editing),
        }
    }

    fn run(&mut self, action: Pending, force: bool) {
        match action {
            Pending::Kill(targets) => self.kill(targets),
            Pending::Delete { sessions, worktrees, .. } => self.delete(sessions, worktrees, force),
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
            KeyCode::Tab | KeyCode::BackTab => {
                let to = if self.view == View::Processes { View::Storage } else { View::Processes };
                self.switch_view(to);
            }
            KeyCode::Char('1') => self.switch_view(View::Processes),
            KeyCode::Char('2') => self.switch_view(View::Storage),
            KeyCode::Char('r') | KeyCode::F(5) => {
                if self.view == View::Storage {
                    self.refresh_storage();
                } else {
                    self.refresh();
                }
            }
            KeyCode::Char('?') | KeyCode::Char('h') => self.mode = Mode::Help,
            KeyCode::Char('s') => self.mode = Mode::Settings { field: 0, editing: None },
            KeyCode::Char('t') => self.cycle_theme(true),
            KeyCode::Char('T') => self.cycle_theme(false),
            KeyCode::Char('x') | KeyCode::Delete if self.view == View::Storage => self.ask_delete(),
            KeyCode::Char('x') | KeyCode::Delete => self.kill_selected(),
            KeyCode::Char(' ') if self.view == View::Storage => self.toggle_mark(),
            KeyCode::Char('M') if self.view == View::Storage => self.mark_old_sessions(),
            KeyCode::Char('u') if self.view == View::Storage => self.marked.clear(),
            KeyCode::Char('X') if self.view == View::Processes => self.ask_kill_all(),
            KeyCode::Char('i') if self.view == View::Processes => self.ask_end_idle(),
            KeyCode::Char('p') if self.view == View::Processes => self.toggle_protect(),
            KeyCode::Char('c') => self.copy_selected(),
            KeyCode::Char('o') => self.open_selected_folder(),
            _ => {}
        }
    }

    fn cycle_theme(&mut self, forward: bool) {
        let next = themes::cycle(&self.cfg.theme, forward);
        self.cfg.theme = next.name.into();
        self.save();
        self.say(format!("{}  ·  {}  (t / T to keep browsing)", next.hello, next.blurb));
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
                self.mode = Mode::Confirm { question, action: Pending::Kill(targets) };
            }
            _ => {}
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
        self.mode = Mode::Confirm { question, action: Pending::Kill(procs.iter().map(Target::from).collect()) };
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
        self.mode = Mode::Confirm { question, action: Pending::Kill(targets) };
    }

    fn toggle_mark(&mut self) {
        let Some(key) = self.selected_key.clone() else { return };
        if !(key.starts_with("wt:") || key.starts_with("sd:")) {
            return;
        }
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
        self.move_selection(1);
    }

    fn mark_old_sessions(&mut self) {
        let Some(s) = &self.storage else { return };
        let old: Vec<String> =
            s.sessions.iter().filter(|d| !d.open && d.age.unwrap_or(0) >= 30 * 86400).map(|d| format!("sd:{}", d.id)).collect();
        if old.is_empty() {
            self.say("No session has been untouched for 30 days");
            return;
        }
        let n = old.len();
        self.marked.extend(old);
        self.say(format!("Marked {} untouched for 30+ days · x to delete", plural(n, "session")));
    }

    fn ask_delete(&mut self) {
        if self.deleting {
            return;
        }
        let Some(s) = self.storage.clone() else { return };
        let mut keys = self.marked.clone();
        if keys.is_empty() {
            match &self.selected_key {
                Some(k) if k.starts_with("wt:") || k.starts_with("sd:") => {
                    keys.insert(k.clone());
                }
                _ => return,
            }
        }
        let (_, open_cwds) = self.open_sessions();
        let mut sessions: Vec<SessionData> = s.sessions.iter().filter(|d| keys.contains(&format!("sd:{}", d.id))).cloned().collect();
        let mut worktrees: Vec<Worktree> = s.worktrees.iter().filter(|w| keys.contains(&format!("wt:{}", w.path))).cloned().collect();

        let mut skipped = Vec::new();
        let open = sessions.iter().filter(|d| d.open).count();
        if open > 0 {
            skipped.push(format!("{} open (close {} first)", plural(open, "session"), if open == 1 { "it" } else { "them" }));
            sessions.retain(|d| !d.open);
        }
        let in_use = worktrees.iter().filter(|w| open_cwds.iter().any(|c| inside(c, &w.path))).count();
        if in_use > 0 {
            skipped.push(format!("{} with an open session in it", plural(in_use, "worktree")));
            worktrees.retain(|w| !open_cwds.iter().any(|c| inside(c, &w.path)));
        }
        if sessions.is_empty() && worktrees.is_empty() {
            self.say(format!("Nothing to delete: {}", skipped.join(", ")));
            return;
        }

        let session_bytes: u64 = sessions.iter().map(|d| d.bytes).sum();
        let wt_bytes: u64 = worktrees.iter().filter_map(|w| w.bytes).sum();
        let risky: Vec<&Worktree> = worktrees.iter().filter(|w| w.risky()).collect();
        let mut q = Vec::new();
        let mut what = Vec::new();
        if !sessions.is_empty() {
            what.push(format!("the data of {} ({})", plural(sessions.len(), "session"), format_bytes(session_bytes)));
        }
        if !worktrees.is_empty() {
            what.push(format!("{} ({})", plural(worktrees.len(), "worktree"), format_bytes(wt_bytes)));
        }
        q.push(format!("Delete {}?", what.join(" and ")));
        q.push(String::new());
        if !sessions.is_empty() {
            let resumable = sessions.iter().filter(|d| d.has_transcript()).count();
            q.push("Session data goes to the Recycle Bin.".into());
            if resumable > 0 {
                q.push(format!("{} can no longer be resumed with `claude --resume` after this.", plural(resumable, "session")));
            }
        }
        if !worktrees.is_empty() {
            q.push("Worktree folders are deleted for good (git worktree remove). Branches are kept.".into());
            for w in worktrees.iter().take(4) {
                q.push(format!("  • {} ⑂ {}  {}", w.repo_name(), w.name(), w.bytes.map(format_bytes).unwrap_or_else(|| "size unknown".into())));
            }
            if worktrees.len() > 4 {
                q.push(format!("  • and {} more", worktrees.len() - 4));
            }
        }
        if !risky.is_empty() {
            q.push(String::new());
            q.push(format!(
                "⚠ {} uncommitted changes, commits on no branch, or a lock.",
                if risky.len() == 1 { "1 worktree has".to_string() } else { format!("{} worktrees have", risky.len()) }
            ));
            q.push("  y leaves those alone; F removes them anyway and that work is lost.".into());
        }
        if !skipped.is_empty() {
            q.push(format!("Skipping {}.", skipped.join(", ")));
        }
        let risky = risky.len();
        self.mode = Mode::Confirm { question: q.join("\n"), action: Pending::Delete { sessions, worktrees, risky } };
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
        let text = if let Some(p) = self.selected_proc(&kind) {
            p.command.clone()
        } else {
            match (kind, &self.result, &self.storage) {
                (RowKind::Orphan(g), Some(r), _) => r.orphans[g].id.clone(),
                (RowKind::Live(g), Some(r), _) => r.live[g].id.clone(),
                (RowKind::Worktree(i), _, Some(s)) => s.worktrees[i].path.clone(),
                (RowKind::SessionData(i), _, Some(s)) => s.sessions[i].id.clone(),
                _ => return,
            }
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self.say(self.theme().copied),
            Err(e) => self.say(format!("Could not copy: {e}")),
        }
    }

    fn open_selected_folder(&mut self) {
        let Some(kind) = self.selected_kind() else { return };
        let folder = match (&kind, &self.storage) {
            (RowKind::Worktree(i), Some(s)) => Some(s.worktrees[*i].path.clone()),
            (RowKind::SessionData(i), Some(s)) => s.sessions[*i].cwd.clone(),
            _ => self.selected_proc(&kind).map(|p| p.cwd.clone()),
        };
        let Some(folder) = folder.filter(|c| !c.is_empty() && std::path::Path::new(c).is_dir()) else {
            self.say("No folder on disk for this row");
            return;
        };
        if Command::new("explorer.exe").arg(&folder).spawn().is_err() {
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
            4 => app.cfg.theme = themes::cycle(&app.cfg.theme, up).name.into(),
            _ => {}
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => return,
            KeyCode::Up | KeyCode::Char('k') => field = field.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => field = (field + 1).min(4),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
                change(self, field, false);
                self.after_settings_change(field);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') => {
                change(self, field, true);
                self.after_settings_change(field);
            }
            KeyCode::Char(' ') | KeyCode::Enter if field == 2 || field == 4 => {
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

/// `path` is `root` or a folder inside it (case-insensitive, as Windows paths are).
fn inside(path: &str, root: &str) -> bool {
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (p, r) = (norm(path), norm(root));
    p == r || p.starts_with(&format!("{r}\\"))
}

fn kind_prefix(kind: &RowKind) -> &'static str {
    match kind {
        RowKind::Orphan(_) => "og",
        RowKind::OrphanProc(..) => "op",
        RowKind::Session(_) => "s",
        RowKind::SessionProc(..) => "sp",
        RowKind::Live(_) => "lg",
        RowKind::LiveProc(..) => "lp",
        RowKind::Worktree(_) => "wt",
        RowKind::SessionData(_) => "sd",
        RowKind::Section => "sec",
        RowKind::Note => "note",
    }
}

/// Lays out a row: `left` truncated to fit, `right` against the right edge. Widths are display
/// columns, so wide characters (kaomoji, CJK) line up too.
fn fit(left: Vec<Span<'static>>, right: Span<'static>, width: usize) -> Line<'static> {
    let room = width.saturating_sub(right.width());
    let mut used = 0;
    let mut spans = Vec::new();
    for span in left {
        let w = span.width();
        if used + w <= room {
            used += w;
            spans.push(span);
            continue;
        }
        let budget = room.saturating_sub(used + 1);
        let mut text = String::new();
        let mut taken = 0;
        for c in span.content.chars() {
            let cw = c.width().unwrap_or(0);
            if taken + cw > budget {
                break;
            }
            text.push(c);
            taken += cw;
        }
        if room > used {
            spans.push(Span::styled(format!("{text}…"), span.style));
            used += taken + 1;
        }
        break;
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
