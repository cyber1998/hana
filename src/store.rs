//! Settings, a full scan, and killing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::scanner::{self, Proc, RunningSession, SessionGroup, Snapshot};
use crate::os;

pub const MINIMUM_IDLE_LIMIT: u64 = 30;
/// In live sessions, processes younger than this are hidden; those are ordinary tool calls.
pub const MIN_LIVE_AGE: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Commands containing any of these are never touched by "kill session", "kill all" or "end idle".
    pub protected_patterns: Vec<String>,
    /// Seconds between scans.
    pub refresh_interval: u64,
    /// A session waiting for input this long counts as idle. Never under half an hour.
    pub idle_limit_minutes: u64,
    /// While on, sessions idle past the limit are ended without asking, on every scan.
    pub ends_idle_sessions: bool,
    /// Look of the interactive view; one of the names in `tui::THEMES`.
    pub theme: String,
    /// Look for a newer release on GitHub, at most once a day.
    pub check_for_updates: bool,
    /// Show the blooming flower for a moment before the interactive view opens.
    pub show_intro: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            protected_patterns: vec!["wsl.exe".into()],
            refresh_interval: 30,
            idle_limit_minutes: 120,
            ends_idle_sessions: false,
            theme: crate::themes::DEFAULT.name.into(),
            check_for_updates: true,
            show_intro: true,
        }
    }
}

impl Config {
    fn dir(name: &str) -> PathBuf {
        let base = os::config_base();
        base.join(name).join("config.json")
    }

    pub fn path() -> PathBuf {
        Self::dir("hana")
    }

    pub fn load() -> Config {
        let read = |path: PathBuf| fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Config>(&t).ok());
        // Settings from before the rename carry over.
        let mut cfg = read(Self::path()).or_else(|| read(Self::dir("seshka"))).unwrap_or_default();
        cfg.idle_limit_minutes = cfg.idle_limit_minutes.max(MINIMUM_IDLE_LIMIT);
        cfg.refresh_interval = cfg.refresh_interval.clamp(5, 3600);
        // Old theme names ("original", "classic") map to their current one.
        cfg.theme = crate::themes::by_name(&cfg.theme).name.into();
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }

    pub fn is_protected(&self, command: &str) -> bool {
        let command = command.to_lowercase();
        self.protected_patterns.iter().any(|p| !p.trim().is_empty() && command.contains(&p.trim().to_lowercase()))
    }

    pub fn protect(&mut self, proc: &Proc) {
        let name = proc.executable();
        if !self.protected_patterns.iter().any(|p| p.eq_ignore_ascii_case(&name)) {
            self.protected_patterns.push(name);
        }
    }

    pub fn unprotect(&mut self, proc: &Proc) {
        let command = proc.command.to_lowercase();
        self.protected_patterns.retain(|p| p.trim().is_empty() || !command.contains(&p.trim().to_lowercase()));
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanResult {
    /// Processes whose Claude Code session has closed, by session.
    pub orphans: Vec<SessionGroup>,
    /// Every open session, the longest idle first.
    pub sessions: Vec<RunningSession>,
    /// Background processes of live sessions that could not be matched to an open one.
    pub live: Vec<SessionGroup>,
}

impl ScanResult {
    pub fn orphan_count(&self) -> usize {
        self.orphans.iter().map(|g| g.procs.len()).sum()
    }
    pub fn orphan_memory(&self) -> u64 {
        self.orphans.iter().map(|g| g.memory()).sum()
    }
    pub fn killable_orphans(&self, cfg: &Config) -> Vec<Proc> {
        self.orphans.iter().flat_map(|g| g.procs.iter()).filter(|p| !cfg.is_protected(&p.command)).cloned().collect()
    }
    pub fn idle_sessions(&self, cfg: &Config) -> Vec<RunningSession> {
        let now = scanner::now_ms();
        self.sessions
            .iter()
            .filter(|s| s.idle_seconds(now).is_some_and(|idle| idle >= cfg.idle_limit_minutes * 60))
            .cloned()
            .collect()
    }

    /// Takes processes that are known to be gone off the lists, before the next scan confirms it.
    pub fn remove(&mut self, gone: &HashSet<u32>) {
        let strip = |groups: &mut Vec<SessionGroup>| {
            for g in groups.iter_mut() {
                g.procs.retain(|p| !gone.contains(&p.pid));
            }
            groups.retain(|g| !g.procs.is_empty());
        };
        strip(&mut self.orphans);
        strip(&mut self.live);
        self.sessions.retain(|s| !gone.contains(&s.process.pid));
        for s in &mut self.sessions {
            s.procs.retain(|p| !gone.contains(&p.pid));
        }
    }
}

pub fn full_scan() -> ScanResult {
    let snap = Snapshot::take();
    let procs = scanner::scan(&snap);
    let now = scanner::now_ms();
    let mut sessions = scanner::sessions(&snap, &procs, MIN_LIVE_AGE);
    sessions.sort_by(|a, b| {
        let key = |s: &RunningSession| (s.idle_seconds(now).map_or(-1i64, |v| v as i64), s.process.age);
        key(b).cmp(&key(a))
    });
    let matched: HashSet<&String> = sessions.iter().filter_map(|s| s.session_id.as_ref()).collect();
    let live_procs: Vec<Proc> =
        procs.iter().filter(|p| p.session_alive && p.age >= MIN_LIVE_AGE && !matched.contains(&p.session_id)).cloned().collect();
    let orphan_procs: Vec<Proc> = procs.iter().filter(|p| !p.session_alive).cloned().collect();
    ScanResult { orphans: scanner::group(orphan_procs), live: scanner::group(live_procs), sessions }
}

/// A process to kill, as the user saw it.
#[derive(Clone, Debug)]
pub struct Target {
    pub pid: u32,
    pub created: u64,
}

impl From<&Proc> for Target {
    fn from(p: &Proc) -> Self {
        Target { pid: p.pid, created: p.created }
    }
}

#[derive(Debug, Default)]
pub struct KillReport {
    /// Requested PIDs that are gone now.
    pub gone: Vec<u32>,
    /// Requested PIDs still running.
    pub survivors: Vec<u32>,
    /// Every PID that went, requested or a descendant.
    pub all_gone: Vec<u32>,
    /// Private bytes the gone processes were holding.
    pub freed: u64,
}

/// Ends the targets and everything they started.
///
/// Windows has no SIGTERM, and ending a process does not end its children. So: windowed apps are first
/// asked to close (WM_CLOSE) and given `grace` to do so; then whatever is left, targets and descendants
/// alike, is ended. Descendants whose command matches a protected pattern are left alone.
pub fn kill(targets: &[Target], cfg: &Config, grace: Duration) -> KillReport {
    let snap = Snapshot::take();
    // PIDs get reused: only kill the process the user saw.
    let same: Vec<&Target> =
        targets.iter().filter(|t| snap.infos.get(&t.pid).is_some_and(|i| t.created == 0 || i.created == t.created)).collect();

    let children = snap.children();
    let mut order: Vec<(u32, u64)> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = same.iter().map(|t| t.pid).collect();
    for t in &same {
        seen.insert(t.pid);
        order.push((t.pid, snap.infos[&t.pid].created));
    }
    // Parents first, so a supervisor cannot restart a child that was just ended.
    while let Some(pid) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            if child == snap.me || !seen.insert(child) {
                continue;
            }
            let info = &snap.infos[&child];
            if cfg.is_protected(&info.cmdline) {
                continue;
            }
            order.push((child, info.created));
            queue.push_back(child);
        }
    }

    // Politely first: close windows on Windows, SIGTERM on Linux.
    let asked = os::request_close(&order);
    if !asked.is_empty() {
        let asked: HashSet<u32> = asked.into_iter().collect();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && order.iter().any(|&(pid, c)| asked.contains(&pid) && os::is_running(pid, c)) {
            thread::sleep(Duration::from_millis(100));
        }
    }
    for &(pid, created) in &order {
        if os::is_running(pid, created) {
            os::terminate(pid, created);
        }
    }
    // Give the system a moment to remove them before the next scan.
    let reaped = Instant::now() + Duration::from_secs(1);
    while Instant::now() < reaped && order.iter().any(|&(pid, c)| os::is_running(pid, c)) {
        thread::sleep(Duration::from_millis(50));
    }

    let memory: HashMap<u32, u64> = order.iter().map(|&(pid, _)| (pid, snap.infos[&pid].memory)).collect();
    let mut report = KillReport::default();
    for &(pid, created) in &order {
        if !os::is_running(pid, created) {
            report.all_gone.push(pid);
            report.freed += memory[&pid];
        }
    }
    for t in targets {
        if report.all_gone.contains(&t.pid) || !os::is_running(t.pid, t.created) {
            report.gone.push(t.pid);
        } else {
            report.survivors.push(t.pid);
        }
    }
    report
}

pub fn format_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else if mb >= 1.0 {
        format!("{mb:.0} MB")
    } else if bytes == 0 {
        "–".into()
    } else {
        "<1 MB".into()
    }
}

/// "30 min", "2 h", "2 h 30 min".
pub fn format_limit(minutes: u64) -> String {
    let (hours, rest) = (minutes / 60, minutes % 60);
    match (hours, rest) {
        (0, r) => format!("{r} min"),
        (h, 0) => format!("{h} h"),
        (h, r) => format!("{h} h {r} min"),
    }
}

pub fn human_age(seconds: u64) -> String {
    if seconds >= 86400 {
        format!("{}d", seconds / 86400)
    } else if seconds >= 3600 {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else if word.ends_with("ss") {
        format!("{n} {word}es")
    } else {
        format!("{n} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Ending a process also ends what it started: neither Windows nor Linux does that by itself.
    /// Only touches processes this test spawns.
    #[test]
    fn kill_takes_descendants_along() {
        // A shell that starts a long-running child: cmd with ping on Windows, sh with sleep on Linux.
        #[cfg(windows)]
        let (mut command, child_name) = {
            let mut c = Command::new("cmd.exe");
            c.args(["/c", "ping -n 60 127.0.0.1 >nul"]);
            (c, "ping.exe")
        };
        #[cfg(not(windows))]
        let (mut command, child_name) = {
            let mut c = Command::new("sh");
            // The trailing `true` keeps sh from replacing itself with sleep.
            c.args(["-c", "sleep 60; true"]);
            (c, "sleep")
        };
        let mut parent = command.stdout(Stdio::null()).spawn().expect("spawn shell");
        let pid = parent.id();
        // Wait for the shell to start its child.
        let deadline = Instant::now() + Duration::from_secs(5);
        let child = loop {
            let snap = Snapshot::take();
            if let Some(&c) = snap.children().get(&pid).and_then(|c| c.iter().find(|&&c| snap.infos[&c].name == child_name)) {
                break (c, snap.infos[&c].created);
            }
            assert!(Instant::now() < deadline, "ping never started");
            thread::sleep(Duration::from_millis(100));
        };
        let created = os::inspect(pid, false).created;
        assert!(os::is_running(child.0, child.1));

        let report = kill(&[Target { pid, created }], &Config::default(), Duration::from_millis(200));
        assert_eq!(report.gone, vec![pid]);
        assert!(report.all_gone.contains(&child.0), "child left running: {report:?}");
        assert!(!os::is_running(child.0, child.1));
        let _ = parent.wait();
    }

    #[test]
    fn protected_patterns_match_case_insensitively() {
        let cfg = Config { protected_patterns: vec!["WSL.exe".into(), " ".into()], ..Config::default() };
        assert!(cfg.is_protected("wsl.exe -d Ubuntu"));
        assert!(!cfg.is_protected("node.exe server.js"));
    }
}
