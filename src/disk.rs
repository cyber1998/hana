//! What Claude Code keeps on disk for each session, and the git worktrees its sessions work in.
//!
//! Per session, in Claude Code's data folder (see scanner::claude_dir):
//!   projects/<project>/<id>.jsonl                  the transcript (`claude --resume` reads it)
//!   projects/<project>/<id>/                       subagent transcripts and large tool output
//!   projects/<project>/<id>.desktop-released.json  a marker left by Claude Desktop
//!   file-history/<id>/                             file backups for rewinding edits
//!   session-env/<id>/                              environment captured for the session
//!   todos/<id>-*.json, debug/<id>.txt              older versions keep these too
//! A project's memory/ folder belongs to every session of that project, so it is never touched.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde::Serialize;

use crate::scanner::{self, claude_dir, Snapshot, UNKNOWN_PROJECT, WORKTREES_MARKER};
use crate::os;

#[derive(Clone, Debug, Serialize)]
pub struct Part {
    pub kind: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionData {
    pub id: String,
    pub project: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// Seconds since any of its files last changed.
    pub age: Option<u64>,
    pub parts: Vec<Part>,
    pub bytes: u64,
    /// The session is open right now; its data is not offered for deletion.
    pub open: bool,
}

impl SessionData {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
    pub fn has_transcript(&self) -> bool {
        self.parts.iter().any(|p| p.kind == "transcript")
    }
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

/// Total size of a folder's files. Links and junctions are not followed, so nothing is counted twice
/// and a junction pointing back up cannot loop.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Every session that has data on disk, the biggest first.
pub fn scan_sessions(open: &HashSet<String>) -> Vec<SessionData> {
    let root = claude_dir();
    let mut parts: HashMap<String, Vec<(Part, Option<SystemTime>)>> = HashMap::new();
    let mut add = |id: &str, kind: &'static str, path: PathBuf, is_dir: bool| {
        let bytes = if is_dir { dir_size(&path) } else { fs::metadata(&path).map(|m| m.len()).unwrap_or(0) };
        let when = modified(&path);
        parts.entry(id.to_string()).or_default().push((Part { kind, path, bytes }, when));
    };

    if let Ok(projects) = fs::read_dir(root.join("projects")) {
        for project in projects.flatten().filter(|p| p.file_type().is_ok_and(|t| t.is_dir())) {
            let Ok(entries) = fs::read_dir(project.path()) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                if let Some(id) = name.strip_suffix(".jsonl").filter(|id| is_uuid(id) && !is_dir) {
                    add(id, "transcript", entry.path(), false);
                } else if let Some(id) = name.strip_suffix(".desktop-released.json").filter(|id| is_uuid(id)) {
                    add(id, "desktop marker", entry.path(), false);
                } else if is_dir && is_uuid(&name) {
                    add(&name, "subagents & tool output", entry.path(), true);
                }
            }
        }
    }
    for (folder, kind) in [("file-history", "file history"), ("session-env", "session env")] {
        let Ok(entries) = fs::read_dir(root.join(folder)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_uuid(&name) {
                add(&name, kind, entry.path(), entry.file_type().is_ok_and(|t| t.is_dir()));
            }
        }
    }
    for (folder, kind) in [("todos", "todos"), ("debug", "debug log")] {
        let Ok(entries) = fs::read_dir(root.join(folder)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.get(..36).filter(|id| is_uuid(id)) {
                add(id, kind, entry.path(), entry.file_type().is_ok_and(|t| t.is_dir()));
            }
        }
    }

    let now = SystemTime::now();
    let mut out: Vec<SessionData> = parts
        .into_iter()
        .map(|(id, parts)| {
            let meta = scanner::session_meta(&id);
            let latest = parts.iter().filter_map(|(_, when)| *when).max();
            let parts: Vec<Part> = parts.into_iter().map(|(p, _)| p).collect();
            let project = if meta.project == UNKNOWN_PROJECT && !parts.iter().any(|p| p.kind == "transcript") {
                "no transcript".to_string()
            } else {
                meta.project
            };
            SessionData {
                bytes: parts.iter().map(|p| p.bytes).sum(),
                age: latest.and_then(|t| now.duration_since(t).ok()).map(|d| d.as_secs()),
                open: open.contains(&id),
                id,
                project,
                title: meta.title,
                cwd: meta.cwd,
                parts,
            }
        })
        .collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.id.cmp(&b.id)));
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct Worktree {
    /// The repository's main folder.
    pub repo: String,
    pub path: String,
    /// The checked-out branch, or None when HEAD is detached.
    pub branch: Option<String>,
    pub head: String,
    /// Known to git; a stray folder under .claude\worktrees is not.
    pub registered: bool,
    pub exists: bool,
    pub locked: bool,
    /// Filled in by `measure`, which is slow for big worktrees.
    pub bytes: Option<u64>,
    /// Changed or untracked files.
    pub dirty: Option<usize>,
    /// Commits reachable only from this worktree's detached HEAD; removing it orphans them.
    pub only_here: Option<usize>,
    /// Sessions that worked in it.
    pub sessions: Vec<String>,
}

impl Worktree {
    pub fn name(&self) -> &str {
        scanner::file_name(&self.path)
    }
    pub fn repo_name(&self) -> &str {
        scanner::file_name(&self.repo)
    }
    /// Removing it would lose work: uncommitted changes, commits nothing else points at, or a lock.
    pub fn risky(&self) -> bool {
        self.dirty.unwrap_or(0) > 0 || self.only_here.unwrap_or(0) > 0 || self.locked
    }
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// A path in one comparable form: this system's separators, no trailing one, and lowercase where
/// the system ignores case.
pub fn norm(path: &str) -> String {
    let p = os::native(path);
    let p = p.trim_end_matches(os::SEP);
    if os::CASE_INSENSITIVE {
        p.to_lowercase()
    } else {
        p.to_string()
    }
}

/// Worktrees of every repository that a session worked in (other than each repository's main folder).
pub fn find_worktrees(cwds: &[String]) -> Vec<Worktree> {
    let mut candidates: Vec<String> = Vec::new();
    let mut seen_candidates = HashSet::new();
    for cwd in cwds {
        let candidate = scanner::worktree_of(cwd).map(|(repo, _)| repo).unwrap_or_else(|| cwd.clone());
        if seen_candidates.insert(norm(&candidate)) && Path::new(&candidate).is_dir() {
            candidates.push(candidate);
        }
    }

    let mut repos = HashSet::new();
    let mut out: Vec<Worktree> = Vec::new();
    for candidate in candidates {
        let Some(common) = git(&candidate, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) else { continue };
        if !repos.insert(norm(common.trim())) {
            continue;
        }
        let Some(list) = git(&candidate, &["worktree", "list", "--porcelain"]) else { continue };
        let mut blocks = list.split("\n\n").filter(|b| !b.trim().is_empty());
        let Some(main) = blocks.next().and_then(|b| b.lines().find_map(|l| l.strip_prefix("worktree "))) else { continue };
        let main = os::native(main);
        let mut registered = HashSet::new();
        for block in blocks {
            let mut wt = Worktree {
                repo: main.clone(),
                path: String::new(),
                branch: None,
                head: String::new(),
                registered: true,
                exists: false,
                locked: false,
                bytes: None,
                dirty: None,
                only_here: None,
                sessions: Vec::new(),
            };
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    wt.path = os::native(p);
                } else if let Some(h) = line.strip_prefix("HEAD ") {
                    wt.head = h.to_string();
                } else if let Some(b) = line.strip_prefix("branch ") {
                    wt.branch = Some(b.trim_start_matches("refs/heads/").to_string());
                } else if line == "locked" || line.starts_with("locked ") {
                    wt.locked = true;
                }
            }
            if wt.path.is_empty() || block.lines().any(|l| l == "bare") {
                continue;
            }
            wt.exists = Path::new(&wt.path).is_dir();
            registered.insert(norm(&wt.path));
            out.push(wt);
        }
        // Folders git no longer knows about, e.g. after a crash halfway through removing one.
        let folder = format!("{main}{}", WORKTREES_MARKER.trim_end_matches(os::SEP));
        if let Ok(entries) = fs::read_dir(&folder) {
            for entry in entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
                let path = entry.path().to_string_lossy().to_string();
                if !registered.contains(&norm(&path)) {
                    out.push(Worktree {
                        repo: main.clone(),
                        path,
                        branch: None,
                        head: String::new(),
                        registered: false,
                        exists: true,
                        locked: false,
                        bytes: None,
                        dirty: None,
                        only_here: None,
                        sessions: Vec::new(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.repo.cmp(&b.repo).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Links each worktree to the sessions that worked in it.
pub fn link_sessions(worktrees: &mut [Worktree], sessions: &[SessionData]) {
    for wt in worktrees.iter_mut() {
        let root = norm(&wt.path);
        wt.sessions = sessions
            .iter()
            .filter(|s| s.cwd.as_deref().is_some_and(|c| norm(c) == root || norm(c).starts_with(&format!("{root}\\"))))
            .map(|s| s.id.clone())
            .collect();
    }
}

#[derive(Clone, Debug)]
pub struct Measured {
    pub bytes: u64,
    pub dirty: Option<usize>,
    pub only_here: Option<usize>,
}

/// Size and git status of one worktree. Walks every file, so it can take a while.
pub fn measure(wt: &Worktree) -> Measured {
    if !wt.exists {
        return Measured { bytes: 0, dirty: None, only_here: None };
    }
    let bytes = dir_size(Path::new(&wt.path));
    let (dirty, only_here) = status(wt);
    Measured { bytes, dirty, only_here }
}

fn status(wt: &Worktree) -> (Option<usize>, Option<usize>) {
    if !wt.registered || !wt.exists {
        return (None, None);
    }
    let dirty = git(&wt.path, &["status", "--porcelain", "--ignore-submodules=dirty"]).map(|s| s.lines().filter(|l| !l.is_empty()).count());
    // A branch survives the worktree's removal; a detached HEAD's own commits do not.
    let only_here = if wt.branch.is_none() {
        git(&wt.path, &["rev-list", "--count", "HEAD", "--not", "--branches", "--remotes", "--tags"]).and_then(|s| s.trim().parse().ok())
    } else {
        Some(0)
    };
    (dirty, only_here)
}

/// Processes running inside a folder, by name: removing it under them would fail or break them.
pub fn busy_in(path: &str, snap: &Snapshot) -> Vec<String> {
    let root = norm(path);
    let mut names: Vec<String> = snap
        .infos
        .iter()
        .filter(|(&pid, i)| pid != snap.me && !i.cwd.is_empty() && (norm(&i.cwd) == root || norm(&i.cwd).starts_with(&format!("{root}{}", os::SEP))))
        .map(|(pid, i)| format!("{} ({pid})", i.name))
        .collect();
    names.sort();
    names
}

/// What a delete did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub freed: u64,
    pub sessions_deleted: usize,
    pub worktrees_removed: usize,
    pub problems: Vec<String>,
}

/// Moves the data of these sessions to the Recycle Bin (the trash on Linux). Open sessions are skipped.
pub fn delete_sessions(sessions: &[SessionData], open: &HashSet<String>, out: &mut Outcome) {
    let (skip, go): (Vec<&SessionData>, Vec<&SessionData>) = sessions.iter().partition(|s| open.contains(&s.id));
    for s in skip {
        out.problems.push(format!("{} is open; close it first", s.short_id()));
    }
    let paths: Vec<PathBuf> = go.iter().flat_map(|s| s.parts.iter().map(|p| p.path.clone())).filter(|p| p.exists()).collect();
    if paths.is_empty() {
        return;
    }
    match os::recycle(&paths) {
        Ok(()) => {
            out.sessions_deleted += go.len();
            out.freed += go.iter().map(|s| s.bytes).sum::<u64>();
        }
        Err(e) => out.problems.push(format!("Could not delete session data: {e}")),
    }
}

/// Removes a worktree the way `git worktree remove` does. Without `force`, one with uncommitted changes,
/// commits nothing else points at, or a lock is left alone. Folders a process is working in are always left alone.
pub fn remove_worktree(wt: &Worktree, force: bool, snap: &Snapshot, out: &mut Outcome) {
    let label = format!("{} ⑂ {}", wt.repo_name(), wt.name());
    if wt.exists {
        let busy = busy_in(&wt.path, snap);
        if !busy.is_empty() {
            out.problems.push(format!("{label} is in use by {}", busy.join(", ")));
            return;
        }
    }
    if !force {
        // Check again now: the list may be minutes old.
        let (dirty, only_here) = status(wt);
        if dirty.unwrap_or(0) > 0 {
            out.problems.push(format!("{label} has {} uncommitted (use force)", dirty.unwrap_or(0)));
            return;
        }
        if only_here.unwrap_or(0) > 0 {
            out.problems.push(format!("{label} has {} commits on no branch (use force)", only_here.unwrap_or(0)));
            return;
        }
        if wt.locked {
            out.problems.push(format!("{label} is locked (use force)"));
            return;
        }
    }
    let bytes = wt.bytes.unwrap_or(0);
    let result = if !wt.registered {
        force_remove_dir(Path::new(&wt.path))
    } else if !wt.exists {
        run_git(&wt.repo, &["worktree", "prune"])
    } else {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
            if wt.locked {
                args.push("--force");
            }
        }
        args.push(&wt.path);
        run_git(&wt.repo, &args)
    };
    match result {
        Ok(()) => {
            out.worktrees_removed += 1;
            out.freed += bytes;
        }
        Err(e) => out.problems.push(format!("{label}: {e}")),
    }
}

fn run_git(dir: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").trim().to_string())
    }
}

/// Deletes a folder for good, including read-only files (git marks its objects read-only).
fn force_remove_dir(path: &Path) -> Result<(), String> {
    if fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = fs::set_permissions(entry.path(), perms);
                }
            }
        }
    }
    fs::remove_dir_all(path).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageResult {
    pub sessions: Vec<SessionData>,
    pub worktrees: Vec<Worktree>,
}

impl StorageResult {
    pub fn session_bytes(&self) -> u64 {
        self.sessions.iter().map(|s| s.bytes).sum()
    }
    pub fn worktree_bytes(&self) -> u64 {
        self.worktrees.iter().filter_map(|w| w.bytes).sum()
    }
}

/// Session data and worktrees, without worktree sizes (see `measure`).
pub fn scan(open: &HashSet<String>, open_cwds: &[String]) -> StorageResult {
    let sessions = scan_sessions(open);
    let mut cwds: Vec<String> = sessions.iter().filter_map(|s| s.cwd.clone()).collect();
    cwds.extend(open_cwds.iter().cloned());
    let mut worktrees = find_worktrees(&cwds);
    link_sessions(&mut worktrees, &sessions);
    StorageResult { sessions, worktrees }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_session_ids() {
        assert!(is_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!is_uuid("memory"));
        assert!(!is_uuid("123e4567-e89b-12d3-a456-42661417400z"));
    }

    /// Paths are built from the system's temp folder, so the test assumes no particular machine.
    #[test]
    fn finds_worktree_roots() {
        let text = |p: &Path| p.to_string_lossy().to_string();
        let repo = std::env::temp_dir().join("my code").join("demo");
        let root = repo.join(".claude").join("worktrees").join("feature-1a2b3c");
        let (found_repo, found_root) = scanner::worktree_of(&text(&root.join("src"))).unwrap();
        assert_eq!(found_repo, text(&repo));
        assert_eq!(found_root, text(&root));
        assert_eq!(scanner::project_name(&text(&root)), "demo ⑂ feature-1a2b3c");
        assert!(scanner::worktree_of(&text(&repo)).is_none());
        // Windows ignores case in paths; on Linux this is not Claude Code's worktree folder.
        let shouting = repo.join(".Claude").join("Worktrees").join("x");
        assert_eq!(scanner::worktree_of(&text(&shouting)).is_some(), os::CASE_INSENSITIVE);
    }

    /// A worktree made in a scratch repository: found, measured, refused while dirty, removed once clean.
    #[test]
    fn worktree_lifecycle() {
        let base = std::env::temp_dir().join(format!("hana-wt-test-{}", os::current_pid()));
        let _ = force_remove_dir(&base);
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let r = repo.to_string_lossy().to_string();
        let sh = |args: &[&str]| run_git(&r, args).unwrap();
        sh(&["init", "-q"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        sh(&["add", "."]);
        sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "a"]);
        let wt_path = repo.join(".claude").join("worktrees").join("feature-1").to_string_lossy().to_string();
        sh(&["worktree", "add", "-q", "--detach", &wt_path]);

        let found = find_worktrees(&[wt_path.clone()]);
        assert_eq!(found.len(), 1, "{found:?}");
        let mut wt = found[0].clone();
        assert!(wt.registered && wt.exists && wt.branch.is_none());

        fs::write(Path::new(&wt_path).join("new.txt"), "dirty").unwrap();
        let m = measure(&wt);
        assert!(m.bytes > 0);
        assert_eq!(m.dirty, Some(1));
        assert_eq!(m.only_here, Some(0));
        wt.bytes = Some(m.bytes);

        let snap = Snapshot::take();
        let mut out = Outcome::default();
        remove_worktree(&wt, false, &snap, &mut out);
        assert_eq!(out.worktrees_removed, 0);
        assert!(Path::new(&wt_path).exists());

        remove_worktree(&wt, true, &snap, &mut out);
        assert_eq!(out.worktrees_removed, 1, "{:?}", out.problems);
        assert!(!Path::new(&wt_path).exists());
        let _ = force_remove_dir(&base);
    }
}
