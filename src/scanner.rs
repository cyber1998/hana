//! Finds the processes Claude Code sessions started, and the sessions themselves.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;

use crate::win;

const TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Clone, Debug, Serialize)]
pub struct Helper {
    pub pid: u32,
    pub created: u64,
}

/// A process that was started by a Claude Code session.
#[derive(Clone, Debug, Serialize)]
pub struct Proc {
    pub pid: u32,
    /// The parent, or 0 when the parent has exited (Windows does not reparent orphans).
    pub ppid: u32,
    /// Creation time as a FILETIME; together with the PID it identifies the process.
    pub created: u64,
    /// Seconds since the process started.
    pub age: u64,
    pub session_id: String,
    /// True while the Claude Code process that started it is still running.
    pub session_alive: bool,
    pub command: String,
    pub path: String,
    pub cwd: String,
    pub ports: Vec<u16>,
    /// Private bytes, this process and its helpers together.
    pub memory: u64,
    /// Helper processes of the same app (a browser's renderers, a console's conhost), folded into this row.
    pub helpers: Vec<Helper>,
}

pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Splits a Windows command line into the program and the rest.
fn split_argv0(command: &str) -> (&str, &str) {
    let s = command.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (&rest[..end], &rest[end + 1..]),
            None => (rest, ""),
        };
    }
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// Splits arguments on whitespace, keeping quoted parts together (quotes stay in place).
fn tokens(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (i, c) in args.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c.is_whitespace() && !quoted {
            if let Some(s) = start.take() {
                out.push(&args[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&args[s..]);
    }
    out
}

fn is_absolute_path(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

impl Proc {
    pub fn executable(&self) -> String {
        if !self.path.is_empty() {
            return file_name(&self.path).to_string();
        }
        file_name(split_argv0(&self.command).0).to_string()
    }

    /// The command with the program reduced to its name, and absolute paths in the arguments to theirs.
    pub fn short_command(&self) -> String {
        let args = split_argv0(&self.command).1;
        let mut out = self.executable();
        for token in tokens(args) {
            let bare = token.trim_matches('"');
            out.push(' ');
            out.push_str(if is_absolute_path(bare) { file_name(bare.trim_end_matches(['\\', '/'])) } else { token });
        }
        out.replace(['\n', '\r'], " ")
    }

    pub fn pids(&self) -> Vec<u32> {
        std::iter::once(self.pid).chain(self.helpers.iter().map(|h| h.pid)).collect()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionGroup {
    pub id: String,
    pub project: String,
    pub title: Option<String>,
    pub procs: Vec<Proc>,
}

impl SessionGroup {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
    pub fn memory(&self) -> u64 {
        self.procs.iter().map(|p| p.memory).sum()
    }
    pub fn oldest(&self) -> u64 {
        self.procs.iter().map(|p| p.age).max().unwrap_or(0)
    }
}

pub struct Info {
    pub ppid: u32,
    /// Executable name, lowercased.
    pub name: String,
    pub created: u64,
    pub path: String,
    pub cmdline: String,
    pub cwd: String,
    pub env: Option<HashMap<String, String>>,
    pub memory: u64,
}

/// Every process at one moment, with what could be read about each.
pub struct Snapshot {
    pub infos: HashMap<u32, Info>,
    pub now: u64,
    pub me: u32,
    /// Claude Code's own records of its running processes, from ~/.claude/sessions, for PIDs they still describe.
    records: HashMap<u32, Value>,
}

impl Snapshot {
    pub fn take() -> Snapshot {
        let me = win::current_pid();
        let now = win::now_filetime();
        let mut infos = HashMap::new();
        for entry in win::list_processes() {
            // The idle process and the kernel.
            if entry.pid == 0 || entry.pid == 4 {
                continue;
            }
            let found = win::inspect(entry.pid, true);
            infos.insert(
                entry.pid,
                Info {
                    ppid: entry.ppid,
                    name: entry.name.to_lowercase(),
                    created: found.created,
                    path: found.path,
                    cmdline: found.cmdline,
                    cwd: found.cwd,
                    env: found.env,
                    memory: found.memory,
                },
            );
        }
        let mut snap = Snapshot { infos, now, me, records: HashMap::new() };
        snap.records = snap.load_records();
        snap
    }

    /// Claude Code writes ~/.claude/sessions/<pid>.json for each interactive process, with its start time.
    /// PIDs get reused, so a record only counts while that start time matches the running process.
    fn load_records(&self) -> HashMap<u32, Value> {
        let mut out = HashMap::new();
        let Ok(dir) = fs::read_dir(claude_dir().join("sessions")) else { return out };
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(pid) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Some(info) = self.infos.get(&pid) else { continue };
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let Ok(json) = serde_json::from_str::<Value>(&text) else { continue };
            let recorded = match json.get("procStart") {
                Some(Value::String(s)) => s.trim().to_string(),
                Some(Value::Number(n)) => n.to_string(),
                _ => continue,
            };
            if info.created != 0 && recorded == info.created.to_string() {
                out.insert(pid, json);
            }
        }
        out
    }

    /// The parent, if it is still running. Windows keeps the parent's PID after the parent exits, and may
    /// hand that PID to a newer process, so a "parent" created after its child is not the parent.
    pub fn parent(&self, pid: u32) -> Option<u32> {
        let info = self.infos.get(&pid)?;
        let up = info.ppid;
        if up == 0 || up == pid {
            return None;
        }
        let parent = self.infos.get(&up)?;
        if parent.created != 0 && info.created != 0 && parent.created > info.created {
            return None;
        }
        Some(up)
    }

    /// A Claude Code CLI process (not the Claude desktop app, which is also called claude.exe).
    pub fn is_claude(&self, pid: u32) -> bool {
        if self.records.contains_key(&pid) {
            return true;
        }
        let Some(info) = self.infos.get(&pid) else { return false };
        let path = info.path.to_lowercase();
        let cmd = info.cmdline.to_lowercase();
        match info.name.as_str() {
            "claude.exe" | "claude" => {
                let desktop_app = path.contains("\\windowsapps\\claude_") || path.contains("\\anthropicclaude\\") || cmd.contains("--type=");
                !desktop_app
            }
            "node.exe" | "bun.exe" => cmd.contains("@anthropic-ai\\claude-code") || cmd.contains("@anthropic-ai/claude-code"),
            _ => false,
        }
    }

    fn has_claude_ancestor(&self, start: u32) -> bool {
        let mut seen = HashSet::new();
        let mut at = start;
        while let Some(up) = self.parent(at) {
            if !seen.insert(up) {
                return false;
            }
            if self.is_claude(up) {
                return true;
            }
            at = up;
        }
        false
    }

    pub fn age(&self, pid: u32) -> u64 {
        match self.infos.get(&pid) {
            Some(info) if info.created != 0 && info.created < self.now => (self.now - info.created) / TICKS_PER_SECOND,
            _ => 0,
        }
    }

    fn started_no_later(&self, a: u32, b: u32) -> bool {
        match (self.infos.get(&a), self.infos.get(&b)) {
            (Some(x), Some(y)) => x.created == 0 || y.created == 0 || x.created <= y.created,
            _ => false,
        }
    }

    fn make_proc(&self, pid: u32, session_id: String, alive: bool) -> Proc {
        let info = &self.infos[&pid];
        Proc {
            pid,
            ppid: self.parent(pid).unwrap_or(0),
            created: info.created,
            age: self.age(pid),
            session_id,
            session_alive: alive,
            command: if info.cmdline.is_empty() { info.name.clone() } else { info.cmdline.clone() },
            path: info.path.clone(),
            cwd: info.cwd.clone(),
            ports: Vec::new(),
            memory: 0,
            helpers: Vec::new(),
        }
    }

    /// PID → children, using only parent links that are still valid.
    pub fn children(&self) -> HashMap<u32, Vec<u32>> {
        let mut out: HashMap<u32, Vec<u32>> = HashMap::new();
        for &pid in self.infos.keys() {
            if let Some(up) = self.parent(pid) {
                out.entry(up).or_default().push(pid);
            }
        }
        out
    }
}

/// Every process started by a Claude Code session, live or closed.
pub fn scan(snap: &Snapshot) -> Vec<Proc> {
    let me = snap.me;
    let mut pids: Vec<u32> = snap.infos.keys().copied().collect();
    pids.sort_unstable();

    let mut procs: Vec<Proc> = Vec::new();
    for &pid in &pids {
        let Some(env) = &snap.infos[&pid].env else { continue };
        if env.get("CLAUDECODE").map(String::as_str) != Some("1") {
            continue;
        }
        if pid == me || snap.parent(pid) == Some(me) || snap.is_claude(pid) {
            continue;
        }
        let session = env.get("CLAUDE_CODE_SESSION_ID").cloned().unwrap_or_else(|| "no-session".into());
        let claude_pid = env.get("CLAUDE_PID").and_then(|v| v.trim().parse::<u32>().ok());
        let alive = claude_pid.is_some_and(|c| snap.is_claude(c) && snap.started_no_later(c, pid)) || snap.has_claude_ancestor(pid);
        procs.push(snap.make_proc(pid, session, alive));
    }
    if procs.is_empty() {
        return procs;
    }

    // Some processes keep their environment unreadable (sandboxed browser processes, elevated ones),
    // so the marker cannot be seen on them. A process started by a marked one still belongs to that session.
    let marked: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    for &pid in &pids {
        if marked.contains_key(&pid) || pid == me || snap.is_claude(pid) {
            continue;
        }
        let mut owner = None;
        let mut at = pid;
        let mut seen = HashSet::new();
        while let Some(up) = snap.parent(at) {
            if !seen.insert(up) {
                break;
            }
            // Past seshka or another Claude Code process it is no longer this session's doing.
            if up == me || snap.is_claude(up) {
                break;
            }
            if let Some(&hit) = marked.get(&up) {
                owner = Some(hit);
                break;
            }
            at = up;
        }
        if let Some(owner) = owner {
            let (session, alive) = (procs[owner].session_id.clone(), procs[owner].session_alive);
            procs.push(snap.make_proc(pid, session, alive));
        }
    }

    // An app's helper processes are part of the app: one row, not twenty. On Windows that is every
    // Chromium/Electron child (same executable, "--type=" in its command line) and each console's conhost.
    let index: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut fold_into: Vec<Option<usize>> = vec![None; procs.len()];
    for (i, p) in procs.iter().enumerate() {
        let Some(&up) = index.get(&p.ppid) else { continue };
        let conhost = snap.infos[&p.pid].name == "conhost.exe";
        let helper = p.command.contains("--type=") && !p.path.is_empty() && p.path.eq_ignore_ascii_case(&procs[up].path);
        if conhost || helper {
            fold_into[i] = Some(up);
        }
    }
    let root = |mut i: usize| {
        let mut steps = 0;
        while let Some(up) = fold_into[i] {
            i = up;
            steps += 1;
            if steps > procs.len() {
                break;
            }
        }
        i
    };
    let mut helpers: Vec<(usize, Helper)> = Vec::new();
    for i in 0..procs.len() {
        if fold_into[i].is_some() {
            helpers.push((root(i), Helper { pid: procs[i].pid, created: procs[i].created }));
        }
    }
    for (r, helper) in helpers {
        procs[r].helpers.push(helper);
    }
    let mut procs: Vec<Proc> = procs.into_iter().enumerate().filter(|(i, _)| fold_into[*i].is_none()).map(|(_, p)| p).collect();

    let ports = win::listening_ports();
    for p in &mut procs {
        for pid in p.pids() {
            p.memory += snap.infos.get(&pid).map_or(0, |i| i.memory);
            for &port in ports.get(&pid).into_iter().flatten() {
                if !p.ports.contains(&port) {
                    p.ports.push(port);
                }
            }
        }
        p.ports.sort_unstable();
    }
    procs
}

pub fn group(procs: Vec<Proc>) -> Vec<SessionGroup> {
    let mut by_session: HashMap<String, Vec<Proc>> = HashMap::new();
    for p in procs {
        by_session.entry(p.session_id.clone()).or_default().push(p);
    }
    let mut groups: Vec<SessionGroup> = by_session
        .into_iter()
        .map(|(id, mut procs)| {
            procs.sort_by(|a, b| b.age.cmp(&a.age));
            let meta = session_meta(&id);
            SessionGroup { id, project: meta.project, title: meta.title, procs }
        })
        .collect();
    groups.sort_by(|a, b| b.oldest().cmp(&a.oldest()).then_with(|| a.id.cmp(&b.id)));
    groups
}

pub fn claude_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default();
    PathBuf::from(home).join(".claude")
}

pub const UNKNOWN_PROJECT: &str = "unknown project";

#[derive(Clone)]
pub struct SessionMeta {
    pub project: String,
    pub title: Option<String>,
}

/// Project name and title of a session, read from Claude Code's own transcript files.
pub fn session_meta(sid: &str) -> SessionMeta {
    static CACHE: OnceLock<Mutex<HashMap<String, SessionMeta>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(sid) {
        return hit.clone();
    }
    let meta = read_meta(sid);
    // A missing file may appear later for a brand-new session, so only cache real hits.
    if meta.project != UNKNOWN_PROJECT {
        cache.lock().unwrap().insert(sid.to_string(), meta.clone());
    }
    meta
}

fn read_meta(sid: &str) -> SessionMeta {
    let unknown = SessionMeta { project: UNKNOWN_PROJECT.into(), title: None };
    if sid.is_empty() || sid.contains(['\\', '/', '.']) {
        return unknown;
    }
    let root = claude_dir().join("projects");
    let Ok(dirs) = fs::read_dir(&root) else { return unknown };
    for dir in dirs.flatten() {
        let file = dir.path().join(format!("{sid}.jsonl"));
        if !file.is_file() {
            continue;
        }
        let (title, cwd) = read_tail(&file);
        let project = match cwd {
            Some(cwd) if !cwd.is_empty() => file_name(cwd.trim_end_matches(['\\', '/'])).to_string(),
            // Claude Code names each project folder after its path, with every non-alphanumeric character
            // replaced by "-": D:\My Code\hana becomes D--My-Code-hana.
            _ => {
                let name = dir.file_name().to_string_lossy().to_string();
                name.rsplit('-').find(|s| !s.is_empty()).unwrap_or(&name).to_string()
            }
        };
        return SessionMeta { project: if project.is_empty() { "~".into() } else { project }, title };
    }
    unknown
}

/// Transcripts can be hundreds of megabytes, so only the tail is read.
fn read_tail(path: &std::path::Path) -> (Option<String>, Option<String>) {
    let Ok(mut file) = fs::File::open(path) else { return (None, None) };
    let chunk: u64 = 2 * 1024 * 1024;
    let size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(size.saturating_sub(chunk)));
    let mut data = Vec::new();
    if file.read_to_end(&mut data).is_err() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&data);
    let (mut custom, mut ai, mut cwd) = (None, None, None);
    for line in text.lines().rev() {
        let wants_custom = custom.is_none() && line.contains("\"customTitle\"");
        let wants_ai = ai.is_none() && line.contains("\"aiTitle\"");
        let wants_cwd = cwd.is_none() && line.contains("\"cwd\"");
        if !(wants_custom || wants_ai || wants_cwd) {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(line) else { continue };
        let text_of = |key: &str| json.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        if wants_custom {
            custom = text_of("customTitle");
        }
        if wants_ai {
            ai = text_of("aiTitle");
        }
        if wants_cwd {
            cwd = text_of("cwd");
        }
        if (custom.is_some() || ai.is_some()) && cwd.is_some() {
            break;
        }
    }
    (custom.or(ai), cwd)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Status {
    Busy,
    Idle,
    /// A status this version of seshka does not know.
    Other(String),
    /// Claude Code left no record for this process.
    Unknown,
}

/// A Claude Code session that is still open: the claude process and what it has running.
#[derive(Clone, Debug, Serialize)]
pub struct RunningSession {
    pub process: Proc,
    pub session_id: Option<String>,
    pub project: String,
    pub title: Option<String>,
    pub status: Status,
    /// When the session entered its current status, in milliseconds since 1970.
    pub status_since_ms: Option<f64>,
    /// Its background processes older than a minute.
    pub procs: Vec<Proc>,
}

impl RunningSession {
    pub fn memory(&self) -> u64 {
        self.process.memory + self.procs.iter().map(|p| p.memory).sum::<u64>()
    }

    /// How long the session has been waiting for input, or None while it is working.
    pub fn idle_seconds(&self, now_ms: f64) -> Option<u64> {
        if self.status != Status::Idle {
            return None;
        }
        self.status_since_ms.map(|since| ((now_ms - since) / 1000.0).max(0.0) as u64)
    }
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

/// Every open Claude Code session, with the background processes from `procs` that belong to it.
///
/// The record in ~/.claude/sessions/<pid>.json is not a documented interface, so every field is
/// optional here: without it a session still shows, just without a status.
pub fn sessions(snap: &Snapshot, procs: &[Proc], min_age: u64) -> Vec<RunningSession> {
    let mut claude: Vec<u32> = snap.infos.keys().copied().filter(|&pid| snap.is_claude(pid)).collect();
    // A claude process started by another one is part of that session, not a session of its own.
    claude.retain(|&pid| !snap.parent(pid).is_some_and(|up| snap.is_claude(up)));
    claude.sort_unstable();

    claude
        .into_iter()
        .map(|pid| {
            let record = snap.records.get(&pid);
            let field = |key: &str| record.and_then(|r| r.get(key)).and_then(Value::as_str).map(str::to_string);
            let session_id = field("sessionId");
            let info = &snap.infos[&pid];
            let cwd = field("cwd").unwrap_or_else(|| info.cwd.clone());
            let meta = session_id.as_deref().map(session_meta);
            let project = match &meta {
                Some(m) if m.project != UNKNOWN_PROJECT => m.project.clone(),
                _ if !cwd.is_empty() => file_name(cwd.trim_end_matches(['\\', '/'])).to_string(),
                _ => "Claude Code".to_string(),
            };
            let title = field("name").filter(|s| !s.is_empty()).or_else(|| meta.and_then(|m| m.title));
            let status = match field("status").as_deref() {
                Some("busy") => Status::Busy,
                Some("idle") => Status::Idle,
                Some(other) => Status::Other(other.to_string()),
                None => Status::Unknown,
            };
            let since = record.and_then(|r| r.get("statusUpdatedAt")).and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            });
            let mut process = snap.make_proc(pid, session_id.clone().unwrap_or_else(|| "no-session".into()), true);
            process.cwd = cwd;
            process.memory = info.memory;
            let mine = procs
                .iter()
                .filter(|p| p.session_alive && p.age >= min_age && Some(&p.session_id) == session_id.as_ref())
                .cloned()
                .collect();
            RunningSession { process, session_id, project, title, status, status_since_ms: since, procs: mine }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_with(command: &str, path: &str) -> Proc {
        Proc {
            pid: 1,
            ppid: 0,
            created: 0,
            age: 0,
            session_id: String::new(),
            session_alive: false,
            command: command.into(),
            path: path.into(),
            cwd: String::new(),
            ports: vec![],
            memory: 0,
            helpers: vec![],
        }
    }

    #[test]
    fn shortens_commands() {
        let p = proc_with(
            r#""C:\Program Files\nodejs\node.exe" "C:\Users\me\app\node_modules\vite\bin\vite.js" --port 5173"#,
            r"C:\Program Files\nodejs\node.exe",
        );
        assert_eq!(p.executable(), "node.exe");
        assert_eq!(p.short_command(), "node.exe vite.js --port 5173");

        let q = proc_with("python -m http.server 8000", "");
        assert_eq!(q.short_command(), "python -m http.server 8000");
    }

    #[test]
    fn splits_program() {
        assert_eq!(split_argv0(r#""a b\c.exe" x y"#), (r"a b\c.exe", " x y"));
        assert_eq!(split_argv0("c.exe"), ("c.exe", ""));
    }
}
