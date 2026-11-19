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

use crate::bloom;
use crate::disk::{self, Outcome, SessionData, StorageResult, Worktree};
use crate::scanner::{self, Proc, RunningSession, SessionGroup, Snapshot, Status};
use crate::themes::{self, Theme};
use crate::update;
use crate::store::{self, format_bytes, format_limit, human_age, plural, Config, KillReport, ScanResult, Target, MINIMUM_IDLE_LIMIT};

// MARK: state

enum Msg {
    Scanned(ScanResult),
    Killed { targets: Vec<u32>, report: KillReport },
    Storage { generation: u64, result: StorageResult },
    Measured { generation: u64, path: String, measured: disk::Measured },
    MeasuringDone { generation: u64 },
    Deleted(Outcome),
    /// A newer release, if any; `asked` when the user pressed U rather than the daily check.
    UpdateChecked { tag: Option<String>, asked: bool },
    Updated(Result<String, String>),
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
    /// Replace hana.exe with the release with this tag.
    Update(String),
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
    /// A problem to report, shown until the next key press so it is not missed.
    alert: Option<String>,
    /// When the flower for the last successful cleanup started growing.
    bloom: Option<Instant>,
    /// When the view opened; spinners take their phase from it.
    started: Instant,
    /// When open sessions' working / idle status was last re-read.
    status_checked: Instant,
    /// When the running delete began, and what it is deleting.
    deleting_since: Option<(Instant, String)>,
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
    /// Rows whose processes are shown (or hidden) against their default, by key.
    toggled: HashSet<String>,
    /// Tag of a newer release, when one was found.
    update: Option<String>,
    updating: bool,
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
        alert: None,
        bloom: None,
        started: Instant::now(),
        status_checked: Instant::now(),
        deleting_since: None,
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
        toggled: HashSet::new(),
        update: None,
        updating: false,
    };
    app.refresh();
    if app.cfg.check_for_updates {
        app.check_for_update(false);
    }
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
            // Working / idle changes often; re-read just the session records between full scans.
            if self.status_checked.elapsed() >= Duration::from_secs(2) {
                self.status_checked = Instant::now();
                if let Some(r) = &mut self.result {
                    for s in &mut r.sessions {
                        if let Some((status, since)) = scanner::read_status(s.process.pid, s.process.created) {
                            s.status = status;
                            s.status_since_ms = since;
                        }
                    }
                }
            }
            terminal.draw(|f| self.draw(f))?;
            // Quick redraws only while something moves: the flower, or a spinner.
            let tick = if self.bloom.is_some() { 60 } else if self.busy() { 90 } else { 250 };
            if event::poll(Duration::from_millis(tick))? {
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
                    self.alert(format!(
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
                self.deleting_since = None;
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
                    let mut text = vec!["Not everything could be deleted:".to_string(), String::new()];
                    text.extend(outcome.problems.iter().take(6).map(|p| format!("• {p}")));
                    if outcome.problems.len() > 6 {
                        text.push(format!("• and {} more", outcome.problems.len() - 6));
                    }
                    self.alert(text.join("\n"));
                }
                self.marked.clear();
                self.storage_scanning = false;
                self.refresh_storage();
                self.refresh();
            }
            Msg::UpdateChecked { tag, asked } => {
                if asked && tag.is_none() {
                    self.say(format!("hana {} is the latest version", update::current()));
                }
                self.update = tag;
            }
            Msg::Updated(result) => {
                self.updating = false;
                match result {
                    Ok(tag) => {
                        self.update = None;
                        self.say(format!("Updated to {tag}. Restart hana to use it."));
                    }
                    Err(e) => self.alert(format!("The update failed: {e}")),
                }
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
        self.bloom = Some(Instant::now());
    }

    /// Reports something that went wrong; it stays until a key is pressed.
    fn alert(&mut self, text: impl Into<String>) {
        self.alert = Some(text.into());
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
        let bytes: u64 = sessions.iter().map(|d| d.bytes).sum::<u64>() + worktrees.iter().filter_map(|w| w.bytes).sum::<u64>();
        let mut what = Vec::new();
        if !worktrees.is_empty() {
            what.push(plural(worktrees.len(), "worktree"));
        }
        if !sessions.is_empty() {
            what.push(format!("{} of session data", plural(sessions.len(), "session")));
        }
        self.deleting_since = Some((Instant::now(), format!("{} · {}", what.join(" and "), format_bytes(bytes))));
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

    /// Looks for a newer release in the background. `asked`: the user pressed U, so go to GitHub
    /// now and say so if there is nothing new.
    fn check_for_update(&mut self, asked: bool) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::UpdateChecked { tag: update::check(asked), asked });
        });
    }

    fn ask_update(&mut self) {
        if self.updating {
            return;
        }
        match self.update.clone() {
            Some(tag) => {
                let question = format!("Update hana {} to {tag}?\n\nThe new version is checked against the release's checksum first.", update::current());
                self.mode = Mode::Confirm { question, action: Pending::Update(tag) };
            }
            None => {
                self.say("Looking for a newer version…");
                self.check_for_update(true);
            }
        }
    }

    fn install_update(&mut self, tag: String) {
        self.updating = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = update::install(&tag).map(|()| tag);
            if result.is_ok() {
                let _ = update::check(true);
            }
            let _ = tx.send(Msg::Updated(result));
        });
    }

    /// Whether work is running in the background that the user may be waiting on.
    fn busy(&self) -> bool {
        self.deleting
            || self.updating
            || self.storage_scanning
            || self.measuring
            || !self.killing.is_empty()
            || (self.scanning && self.result.is_none())
    }

    /// A spinner frame that moves while hana works, so a long delete never looks stuck.
    fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(self.started.elapsed().as_millis() / 90 % 10) as usize]
    }

    /// What is running, for the header: spinner, what, and for how long when it can take a while.
    fn activity(&self) -> Option<String> {
        let sp = self.spinner();
        let took = |at: &Instant| {
            let s = at.elapsed().as_secs();
            if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) }
        };
        if let Some((at, what)) = &self.deleting_since {
            return Some(format!("{sp} deleting {what} · {}", took(at)));
        }
        if self.updating {
            return Some(format!("{sp} updating hana…"));
        }
        if !self.killing.is_empty() {
            return Some(format!("{sp} ending {}…", plural(self.killing.len(), "process")));
        }
        // A first scan is shown in the empty list itself; only rescans need saying up here.
        if self.view == View::Storage && self.storage_scanning && self.storage.is_some() {
            return Some(format!("{sp} looking through sessions and repositories…"));
        }
        if self.view == View::Storage && self.measuring {
            let (done, all) = self.storage.as_ref().map(|s| (s.worktrees.iter().filter(|w| w.bytes.is_some()).count(), s.worktrees.len())).unwrap_or((0, 0));
            return Some(format!("{sp} measuring worktrees {done}/{all}…"));
        }
        None
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
            rows.push(self.note("scanning", &format!("  {} Looking…", self.spinner()), t.dim));
            return rows;
        };
        let now = scanner::now_ms();

        if r.orphans.is_empty() {
            rows.push(Row {
                kind: RowKind::Section,
                key: "sec:clean".into(),
                line: Line::from(Span::styled(format!(" {}", t.clean.trim()), Style::new().fg(t.good))),
            });
        } else {
            rows.push(self.section("LEFT BEHIND", format!("{} · {}", plural(r.orphan_count(), "process"), format_bytes(r.orphan_memory()))));
            for (g, group) in r.orphans.iter().enumerate() {
                let row = self.group_row(RowKind::Orphan(g), group, true, width);
                let open = self.expanded(&row.key, true);
                rows.push(row);
                if open {
                    for (p, proc) in group.procs.iter().enumerate() {
                        rows.push(self.proc_row(RowKind::OrphanProc(g, p), proc, width));
                    }
                }
            }
        }

        rows.push(Self::gap("gap1"));
        let idle_count = r.idle_sessions(&self.cfg).len();
        let summary = if idle_count > 0 { format!("{} idle over {}", idle_count, format_limit(self.cfg.idle_limit_minutes)) } else { String::new() };
        rows.push(self.section("OPEN SESSIONS", summary));
        if r.sessions.is_empty() {
            rows.push(self.note("nosessions", t.no_sessions, t.dim));
        }
        for (s, session) in r.sessions.iter().enumerate() {
            let row = self.session_row(s, session, now, width);
            let open = self.expanded(&row.key, false);
            rows.push(row);
            if open {
                for (p, proc) in session.procs.iter().enumerate() {
                    rows.push(self.proc_row(RowKind::SessionProc(s, p), proc, width));
                }
            }
        }

        if !r.live.is_empty() {
            rows.push(Self::gap("gap2"));
            rows.push(self.section("IN OTHER SESSIONS", String::new()));
            for (g, group) in r.live.iter().enumerate() {
                let row = self.group_row(RowKind::Live(g), group, false, width);
                let open = self.expanded(&row.key, false);
                rows.push(row);
                if open {
                    for (p, proc) in group.procs.iter().enumerate() {
                        rows.push(self.proc_row(RowKind::LiveProc(g, p), proc, width));
                    }
                }
            }
        }
        rows
    }

    /// Whether a session's processes are listed under it: `default` unless toggled with enter.
    fn expanded(&self, key: &str, default: bool) -> bool {
        default != self.toggled.contains(key)
    }

    /// - when a row's processes are listed under it, + when they are folded away. Plain ASCII: some
    /// terminals draw arrow glyphs two columns wide.
    fn fold_span(&self, open: bool) -> Span<'static> {
        Span::styled(if open { " - " } else { " + " }, Style::new().fg(self.theme().dim))
    }

    fn group_row(&self, kind: RowKind, group: &SessionGroup, open_default: bool, width: usize) -> Row {
        let t = self.theme();
        let key = format!("{}:{}", kind_prefix(&kind), group.id);
        let mut left = vec![self.fold_span(self.expanded(&key, open_default)), Span::styled(group.project.clone(), Style::new().fg(t.text).bold())];
        if let Some(title) = &group.title {
            left.push(Span::styled(format!(" — {title}"), Style::new().fg(t.text)));
        }
        let busy = group.procs.iter().any(|p| self.killing.contains(&p.pid));
        let right = if busy {
            Span::styled(format!("{:>28} ", format!("{} {}", self.spinner(), t.killing)), Style::new().fg(t.accent))
        } else {
            Span::raw(format!("{:>12}{:>10}{:>6} ", plural(group.procs.len(), "proc"), format_bytes(group.memory()), human_age(group.oldest())))
        };
        Row { key, kind, line: fit(left, right, width) }
    }

    fn proc_row(&self, kind: RowKind, proc: &Proc, width: usize) -> Row {
        let t = self.theme();
        let protected = self.cfg.is_protected(&proc.command);
        let ports = proc.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ");
        let killing = self.killing.contains(&proc.pid);
        let right = if killing {
            Span::styled(format!("{:>28} ", format!("{} {}", self.spinner(), t.killing)), Style::new().fg(t.accent))
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

    /// What an open session is doing, as Claude Code reports it in ~/.claude/sessions/<pid>.json:
    /// a short label and its color. Working is green; idle is amber, or the accent once past the
    /// idle limit.
    fn session_state(&self, session: &RunningSession, now: f64) -> (String, Color, bool) {
        let t = self.theme();
        let limit = self.cfg.idle_limit_minutes * 60;
        match (&session.status, session.idle_seconds(now)) {
            (Status::Busy, _) => ("working".into(), t.good, true),
            (Status::Idle, Some(idle)) if idle >= limit => (format!("idle {}", human_age(idle)), t.accent, false),
            (Status::Idle, Some(idle)) => (format!("idle {}", human_age(idle)), t.warn, false),
            (Status::Idle, None) => ("idle".into(), t.warn, false),
            (Status::Other(o), _) => (o.clone(), t.dim, false),
            (Status::Unknown, _) => ("open".into(), t.dim, false),
        }
    }

    /// The open session with this ID, if there is one.
    fn open_session(&self, id: &str) -> Option<&RunningSession> {
        self.result.as_ref()?.sessions.iter().find(|s| s.session_id.as_deref() == Some(id))
    }

    fn session_row(&self, s: usize, session: &RunningSession, now: f64, width: usize) -> Row {
        let t = self.theme();
        let (status, color, working) = self.session_state(session, now);
        let glyph = match session.status {
            Status::Busy => t.busy_dot,
            Status::Idle => t.idle_dot,
            _ => t.unknown_dot,
        };
        // A working session's dot pulses, so it reads as live at a glance.
        let mut dot_style = Style::new().fg(color);
        if working && (self.started.elapsed().as_millis() / 600) % 2 == 1 {
            dot_style = dot_style.add_modifier(Modifier::DIM);
        }
        let key = format!("s:{}:{}", session.process.pid, session.process.created);
        let mut left = vec![
            self.fold_span(self.expanded(&key, false)),
            Span::styled(glyph.trim().to_string(), dot_style),
            Span::raw(" "),
            Span::styled(session.project.clone(), Style::new().fg(t.text).bold()),
        ];
        if let Some(title) = &session.title {
            left.push(Span::styled(format!(" — {title}"), Style::new().fg(t.text)));
        }
        let right = if self.killing.contains(&session.process.pid) {
            vec![Span::styled(format!("{:>28} ", format!("{} {}", self.spinner(), t.ending)), Style::new().fg(t.accent))]
        } else {
            let mut label = Style::new().fg(color);
            if working {
                label = label.add_modifier(Modifier::BOLD);
            }
            vec![
                Span::styled(format!("{status:>12}"), label),
                Span::styled(format!("{:>10}{:>6} ", format_bytes(session.memory()), human_age(session.process.age)), Style::new().fg(t.text)),
            ]
        };
        Row { key, kind: RowKind::Session(s), line: fit_spans(left, right, width) }
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
            rows.push(self.note("scanning", &format!("  {} Looking through ~/.claude and your repositories…", self.spinner()), t.dim));
            return rows;
        };
        let now = scanner::now_ms();
        let (_, open_cwds) = self.open_sessions();

        let measured = s.worktrees.iter().filter(|w| w.bytes.is_some()).count();
        let mut summary = format!("{} · {}", s.worktrees.len(), format_bytes(s.worktree_bytes()));
        if self.measuring {
            summary.push_str(&format!(" · {} {} {measured}/{}", self.spinner(), t.measuring, s.worktrees.len()));
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
            // Sessions open in this worktree: working beats idle, so the tag says the most urgent.
            let here: Vec<&RunningSession> =
                self.result.as_ref().map(|r| r.sessions.iter().filter(|s| inside(&s.process.cwd, &wt.path)).collect()).unwrap_or_default();
            let in_use = !here.is_empty() || open_cwds.iter().any(|c| inside(c, &wt.path));
            let (session_tag, session_color) = if here.iter().any(|s| s.status == Status::Busy) {
                (" session working".to_string(), t.good)
            } else if let Some(s) = here.first() {
                let (label, color, _) = self.session_state(s, now);
                (format!(" session {label}"), color)
            } else {
                (" open session".to_string(), t.good)
            };
            for (show, text, color) in [
                (in_use, session_tag, session_color),
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
                _ if self.deleting && self.marked.contains(&key) => format!("{} {}", self.spinner(), t.deleting),
                Some(b) => format_bytes(b),
                None => self.spinner().into(),
            };
            let sessions = if wt.sessions.is_empty() { String::new() } else { plural(wt.sessions.len(), "session") };
            let right = Span::styled(format!("{:>14}{:>10} ", sessions, size), Style::new().fg(t.text));
            rows.push(Row { kind: RowKind::Worktree(i), key, line: fit(left, right, width) });
        }

        rows.push(Self::gap("gap3"));
        let old = s.sessions.iter().filter(|d| !d.open && d.age.unwrap_or(0) >= 30 * 86400).count();
        let mut summary = format!("{} · {}", plural(s.sessions.len(), "session"), format_bytes(s.session_bytes()));
        if old > 0 {
            summary.push_str(&format!(" · {old} older than 30 days"));
        }
        rows.push(self.section("SESSION DATA", summary));
        if s.sessions.is_empty() {
            rows.push(self.note("nodata", t.no_data, t.dim));
        }
        for (i, data) in s.sessions.iter().enumerate() {
            let key = format!("sd:{}", data.id);
            let mut left = vec![self.mark_span(&key)];
            left.push(Span::styled(data.project.clone(), Style::new().fg(t.text).bold()));
            if let Some(title) = &data.title {
                left.push(Span::styled(format!(" — {title}"), Style::new().fg(t.text)));
            }
            // Open sessions say what they are doing instead of how old they are.
            let (when, when_color, working) = match self.open_session(&data.id) {
                Some(session) => self.session_state(session, now),
                None if data.open => ("open".to_string(), t.dim, false),
                None => (data.age.map(human_age).unwrap_or_default(), t.text, false),
            };
            let right = if self.deleting && self.marked.contains(&key) {
                vec![Span::styled(format!("{:>20} ", format!("{} {}", self.spinner(), t.deleting)), Style::new().fg(t.accent))]
            } else {
                let mut style = Style::new().fg(when_color);
                if working {
                    style = style.add_modifier(Modifier::BOLD);
                }
                vec![
                    Span::styled(format!("{:>10}", format_bytes(data.bytes)), Style::new().fg(if data.open { t.dim } else { t.text })),
                    Span::styled(format!("{when:>10} "), style),
                ]
            };
            rows.push(Row { kind: RowKind::SessionData(i), key, line: fit_spans(left, right, width) });
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
        let [header, body, details, footer] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(4), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());

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
        if let Some(at) = self.bloom {
            if at.elapsed() >= bloom::LENGTH {
                self.bloom = None;
            } else {
                bloom::draw(f.buffer_mut(), body, at.elapsed(), t);
            }
        }

        match &self.mode {
            Mode::Normal => {}
            Mode::Confirm { question, action } => {
                let risky = matches!(action, Pending::Delete { risky, .. } if *risky > 0);
                self.draw_confirm(f, question, risky)
            }
            Mode::Settings { field, editing } => self.draw_settings(f, *field, editing.as_deref()),
            Mode::Help => self.draw_help(f),
        }
        if let Some(text) = &self.alert {
            self.draw_alert(f, text);
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
        let mut spans = vec![logo, Span::raw("  "), tab("processes", View::Processes), Span::raw(" "), tab("storage", View::Storage)];
        // Only say something while work is under way.
        let mut status_color = t.dim;
        let status = match (self.activity(), &self.update) {
            (Some(doing), _) => {
                status_color = t.info;
                doing
            }
            (None, Some(tag)) => {
                status_color = t.accent;
                format!("{tag} available · U")
            }
            (None, None) => String::new(),
        };
        let used: usize = spans.iter().map(Span::width).sum();
        let status_width = Span::raw(status.as_str()).width();
        if used + status_width + 2 < area.width as usize {
            spans.push(Span::raw(" ".repeat(area.width as usize - used - status_width - 2)));
            spans.push(Span::styled(status, Style::new().fg(status_color)));
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
                format!(" {} marked · {}", self.marked.len(), format_bytes(bytes)),
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

    /// Two lines about the selected row: where it is, and what it is.
    fn draw_details(&self, f: &mut Frame, area: Rect, kind: Option<&RowKind>) {
        let t = self.theme();
        let block = Block::new().borders(Borders::TOP).border_type(t.border).border_style(Style::new().fg(t.dim));
        let dim = |s: String| Span::styled(s, Style::new().fg(t.dim));
        let value = |s: String| Span::styled(s, Style::new().fg(t.text));
        let mut lines: Vec<Line> = Vec::new();
        match kind {
            Some(RowKind::Worktree(i)) => {
                if let Some(wt) = self.storage.as_ref().and_then(|s| s.worktrees.get(*i)) {
                    lines.push(Line::from(value(format!(" {}", wt.path))));
                    let checkout = match &wt.branch {
                        Some(b) => format!("branch {b}, kept when removed"),
                        None => format!("detached at {}", &wt.head[..wt.head.len().min(7)]),
                    };
                    let status = match (wt.dirty, wt.only_here) {
                        (Some(0), Some(0)) => "clean, safe to remove".to_string(),
                        (Some(d), o) => format!("{d} uncommitted, {} commits on no branch", o.unwrap_or(0)),
                        _ if !wt.exists => "folder is gone; removing prunes it from git".into(),
                        _ if !wt.registered => "a folder git does not know about".into(),
                        _ => "checking…".into(),
                    };
                    lines.push(Line::from(dim(format!(" {checkout} · {status}"))));
                }
            }
            Some(RowKind::SessionData(i)) => {
                if let Some(d) = self.storage.as_ref().and_then(|s| s.sessions.get(*i)) {
                    lines.push(Line::from(value(format!(" {}", d.cwd.clone().unwrap_or_default()))));
                    let mut second = vec![dim(format!(" {}", d.id))];
                    if d.open {
                        let note = match self.open_session(&d.id).map(|s| &s.status) {
                            Some(Status::Busy) => "  working right now: let it finish and close it to delete its data",
                            _ => "  open: close it to delete its data",
                        };
                        second.push(Span::styled(note, Style::new().fg(t.good)));
                    }
                    lines.push(Line::from(second));
                }
            }
            Some(kind) => {
                if let Some(p) = self.selected_proc(kind) {
                    lines.push(Line::from(vec![value(format!(" {}", p.cwd)), dim(format!("   pid {}", p.pid))]));
                    lines.push(Line::from(dim(format!(" {}", p.command.replace(['\n', '\r'], " ")))));
                } else if let Some(r) = &self.result {
                    let group = match *kind {
                        RowKind::Orphan(g) => r.orphans.get(g),
                        RowKind::Live(g) => r.live.get(g),
                        _ => None,
                    };
                    if let Some(g) = group {
                        lines.push(Line::from(value(format!(" {}", g.project))));
                        lines.push(Line::from(dim(format!(" session {}", g.id))));
                    }
                }
            }
            None => {}
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
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
            View::Processes => &[("enter", "show"), ("x", "kill"), ("X", "kill all"), ("tab", "storage"), ("?", "more"), ("q", "quit")],
            View::Storage => &[("space", "mark"), ("x", "delete"), ("tab", "processes"), ("?", "more"), ("q", "quit")],
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

    fn draw_alert(&self, f: &mut Frame, text: &str) {
        let t = self.theme();
        let width = 72u16;
        // Rough wrapped height: each line of text over the inner width, plus padding and the hint.
        let inner = width as usize - 4;
        let rows: usize = text.lines().map(|l| Span::raw(l).width().div_ceil(inner).max(1)).sum();
        let area = centered(f.area(), width, rows as u16 + 5);
        f.render_widget(Clear, area);
        let mut lines = vec![Line::default()];
        lines.extend(text.lines().map(|l| Line::from(Span::styled(format!(" {l}"), Style::new().fg(t.text)))));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(" any key to close", Style::new().fg(t.dim))));
        let block = Block::bordered().border_type(t.border).style(t.base()).title(" ⚠ ").border_style(Style::new().fg(t.warn));
        f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
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
        let area = centered(f.area(), 64, 11);
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
            Line::from(vec![
                mark(5),
                name("Check for updates"),
                value(if self.cfg.check_for_updates { "[x] once a day".into() } else { "[ ] only with U".into() }),
            ]),
            Line::default(),
            hint(
                if editing.is_some() { "   comma-separated · enter save · esc cancel" } else { "   ↑↓ choose · ←→ change · enter edit · esc close" }.into(),
            ),
        ];
        let block = Block::bordered().border_type(t.border).style(t.base()).title(t.settings_title).border_style(Style::new().fg(t.accent));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let t = self.theme();
        let key = |k: &str, what: &str| {
            Line::from(vec![Span::styled(format!("  {k:<10}"), Style::new().fg(t.accent).bold()), Span::styled(what.to_string(), Style::new().fg(t.text))])
        };
        let head = |s: &str| Line::from(Span::styled(format!(" {s}"), Style::new().fg(t.info).bold()));
        let lines = vec![
            head("processes"),
            key("enter", "show / hide a session's processes"),
            key("x", "kill the selection; on an open session, end it"),
            key("X", "kill everything left behind"),
            key("i", "end sessions idle past the limit"),
            key("p", "protect / unprotect a program"),
            Line::default(),
            head("storage"),
            key("space", "mark · M mark older than 30 days · u unmark all"),
            key("x", "delete marked (session data goes to the Recycle Bin)"),
            Line::default(),
            head("anywhere"),
            key("↑↓ jk", "move · tab switch view · r rescan"),
            key("c / o", "copy / open folder"),
            key("t / T", "next / previous theme"),
            key("s", "settings"),
            key("U", "update hana"),
        ];
        let area = centered(f.area(), 64, lines.len() as u16 + 2);
        f.render_widget(Clear, area);
        let block = Block::bordered().border_type(t.border).style(t.base()).title(t.help_title).border_style(Style::new().fg(t.accent));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    // MARK: keys

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        // A key press dismisses an alert, and does nothing else.
        if self.alert.take().is_some() {
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
            Pending::Delete { sessions, worktrees, .. } => {
                // Rows show "deleting…" while it runs, including a selected row that was not marked.
                self.marked.extend(sessions.iter().map(|d| format!("sd:{}", d.id)));
                self.marked.extend(worktrees.iter().map(|w| format!("wt:{}", w.path)));
                self.delete(sessions, worktrees, force)
            }
            Pending::Update(tag) => self.install_update(tag),
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
            KeyCode::Enter | KeyCode::Right | KeyCode::Left if self.view == View::Processes => self.toggle_fold(key.code),
            KeyCode::Char('U') => self.ask_update(),
            KeyCode::Char('c') => self.copy_selected(),
            KeyCode::Char('o') => self.open_selected_folder(),
            _ => {}
        }
    }

    /// Enter shows or hides a session's processes; → shows, ← hides. On a process row, ← and enter
    /// fold its session and select it.
    fn toggle_fold(&mut self, code: KeyCode) {
        let Some(kind) = self.selected_kind() else { return };
        let parent = match kind {
            RowKind::Orphan(_) | RowKind::Session(_) | RowKind::Live(_) => None,
            RowKind::OrphanProc(g, _) => Some(RowKind::Orphan(g)),
            RowKind::SessionProc(s, _) => Some(RowKind::Session(s)),
            RowKind::LiveProc(g, _) => Some(RowKind::Live(g)),
            _ => return,
        };
        if parent.is_some() && code == KeyCode::Right {
            return;
        }
        let target = parent.unwrap_or(kind);
        let rows = self.rows(200);
        let Some((index, row)) = rows.iter().enumerate().find(|(_, r)| r.kind == target) else { return };
        let open = rows.get(index + 1).is_some_and(|next| {
            matches!(next.kind, RowKind::OrphanProc(..) | RowKind::SessionProc(..) | RowKind::LiveProc(..))
        });
        let want_open = match code {
            KeyCode::Right => true,
            KeyCode::Left => false,
            _ => !open,
        };
        if want_open != open {
            let key = row.key.clone();
            if !self.toggled.remove(&key) {
                self.toggled.insert(key.clone());
            }
            self.selected_key = Some(key);
            self.state.select(Some(index));
        }
    }

    fn cycle_theme(&mut self, forward: bool) {
        let next = themes::cycle(&self.cfg.theme, forward);
        self.cfg.theme = next.name.into();
        self.save();
        self.say(format!("theme: {}", next.name));
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
            5 => app.cfg.check_for_updates = !app.cfg.check_for_updates,
            _ => {}
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => return,
            KeyCode::Up | KeyCode::Char('k') => field = field.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => field = (field + 1).min(5),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
                change(self, field, false);
                self.after_settings_change(field);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') => {
                change(self, field, true);
                self.after_settings_change(field);
            }
            KeyCode::Char(' ') | KeyCode::Enter if matches!(field, 2 | 4 | 5) => {
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
    fit_spans(left, vec![right], width)
}

/// `fit` with a right side of several differently styled pieces.
fn fit_spans(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let room = width.saturating_sub(right.iter().map(Span::width).sum());
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
    spans.extend(right);
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
