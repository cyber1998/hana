//! hana: cleans up after Claude Code on Windows. Based on jigjoy-ai/seshka.

mod bloom;
mod disk;
mod scanner;
mod store;
mod themes;
mod tui;
mod update;
mod win;

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

use store::{format_bytes, human_age, plural, Config, Target};

const HELP: &str = "\
hana - cleans up after Claude Code on Windows

Processes:
  hana [--storage]                     Open the interactive view (on the storage tab)
  hana list [--json]                   Print leftovers and open sessions, then exit
  hana kill-all [-y]                   Kill every leftover process (except protected ones)
  hana kill-idle [-y]                  End sessions idle longer than the limit (see settings)

Storage:
  hana disk [--json]                   How much each worktree and each session's data weighs
  hana delete-session <id>... [-y]     Move sessions' data to the Recycle Bin (ID or its prefix)
  hana remove-worktree <path|name> [--force] [-y]
                                       git worktree remove; --force also drops uncommitted work

Looks:
  hana themes                          Show every theme with its colors
  hana --theme <name>                  Pick a theme (remembered); t / T cycle in the view

Other:
  hana config                          Print where settings are kept, and what they are
  hana update [--check]                Update to the latest release (--check: only say if there is one)
  hana --help | --version

Settings live in %APPDATA%\\hana\\config.json and can be changed from the interactive view (s).
";

fn main() -> ExitCode {
    update::clean_up();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = Config::load();
    // --theme NAME or --theme=NAME; taken out so NAME is not read as a command.
    let theme_at = args.iter().position(|a| a == "--theme" || a.starts_with("--theme="));
    if let Some(at) = theme_at {
        let flag = args.remove(at);
        let name = match flag.strip_prefix("--theme=") {
            Some(name) => Some(name.to_string()),
            None if at < args.len() => Some(args.remove(at)),
            None => None,
        };
        match name.as_deref().and_then(themes::find) {
            Some(theme) => {
                cfg.theme = theme.name.into();
                let _ = cfg.save();
            }
            None => {
                let names: Vec<&str> = themes::THEMES.iter().map(|t| t.name).collect();
                eprintln!("hana: --theme takes one of: {}", names.join(", "));
                return ExitCode::from(2);
            }
        }
    }
    let yes = args.iter().any(|a| a == "-y" || a == "--yes");
    let json = args.iter().any(|a| a == "--json");
    let force = args.iter().any(|a| a == "--force");
    let command = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
    let operands: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).skip(1).map(String::as_str).collect();

    let result = match command {
        _ if args.iter().any(|a| a == "-h" || a == "--help") => {
            print!("{HELP}");
            Ok(())
        }
        _ if args.iter().any(|a| a == "-V" || a == "--version") => {
            println!("hana {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None => tui::run(&mut cfg, args.iter().any(|a| a == "--storage")),
        Some("list") => list(&cfg, json),
        Some("kill-all") => kill_all(&cfg, yes),
        Some("kill-idle") => kill_idle(&cfg, yes),
        Some("disk") => disk_report(json),
        Some("delete-session") => delete_sessions(&operands, yes),
        Some("remove-worktree") => remove_worktree(&operands, force, yes),
        Some("themes") => {
            show_themes(&cfg);
            Ok(())
        }
        Some("update") => self_update(args.iter().any(|a| a == "--check")),
        Some("config") => {
            println!("{}", Config::path().display());
            println!("{}", serde_json::to_string_pretty(&cfg).unwrap_or_default());
            Ok(())
        }
        Some(other) => {
            eprintln!("hana: unknown command '{other}'\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hana: {e}");
            ExitCode::FAILURE
        }
    }
}

fn self_update(check_only: bool) -> io::Result<()> {
    let current = update::current();
    let latest = update::latest_tag().map_err(|e| io::Error::other(format!("could not reach GitHub: {e}")))?;
    if !update::is_newer(&latest, current) {
        println!("hana {current} is the latest version.");
        return Ok(());
    }
    if check_only {
        println!("hana {latest} is available (you have {current}). Run `hana update` to get it.");
        return Ok(());
    }
    println!("Updating hana {current} → {latest}…");
    update::install(&latest).map_err(io::Error::other)?;
    // The next automatic check should not offer what is now installed.
    let _ = update::check(true);
    println!("Done. hana is now {latest}.");
    Ok(())
}

fn group_title(project: &str, title: Option<&str>, id: &str) -> String {
    let short = &id[..id.len().min(8)];
    match title {
        Some(t) => format!("{project} — {t} ({short})"),
        None => format!("{project} ({short})"),
    }
}

fn proc_line(p: &scanner::Proc, cfg: &Config) -> String {
    let ports = p.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ");
    let protected = if cfg.is_protected(&p.command) { "  [protected]" } else { "" };
    format!(
        "    {:>6}  {:>7}  {:>4}  {:<12} {}{}",
        p.pid,
        format_bytes(p.memory),
        human_age(p.age),
        ports,
        p.short_command(),
        protected
    )
}

fn list(cfg: &Config, json: bool) -> io::Result<()> {
    let result = store::full_scan();
    if json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
        return Ok(());
    }
    let n = result.orphan_count();
    if n == 0 {
        println!("Nothing left behind.");
    } else {
        println!(
            "Left behind: {} from {}, holding {}",
            plural(n, "process"),
            plural(result.orphans.len(), "closed session"),
            format_bytes(result.orphan_memory())
        );
        for g in &result.orphans {
            println!("  {}", group_title(&g.project, g.title.as_deref(), &g.id));
            for p in &g.procs {
                println!("{}", proc_line(p, cfg));
            }
        }
    }
    let now = scanner::now_ms();
    if !result.sessions.is_empty() {
        println!("\nOpen sessions: {}", result.sessions.len());
        for s in &result.sessions {
            let status = match (&s.status, s.idle_seconds(now)) {
                (scanner::Status::Idle, Some(idle)) => format!("idle {}", human_age(idle)),
                (scanner::Status::Busy, _) => "working".into(),
                (scanner::Status::Other(o), _) => o.clone(),
                _ => "unknown".into(),
            };
            let id = s.session_id.as_deref().unwrap_or("no-session");
            println!(
                "  [{status}] {}  pid {}  {}",
                group_title(&s.project, s.title.as_deref(), id),
                s.process.pid,
                format_bytes(s.memory())
            );
            for p in &s.procs {
                println!("{}", proc_line(p, cfg));
            }
        }
    }
    if !result.live.is_empty() {
        println!("\nRunning in other open sessions:");
        for g in &result.live {
            println!("  {}", group_title(&g.project, g.title.as_deref(), &g.id));
            for p in &g.procs {
                println!("{}", proc_line(p, cfg));
            }
        }
    }
    Ok(())
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    let _ = io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

fn report(report: &store::KillReport, what: &str) {
    if report.gone.is_empty() && report.survivors.is_empty() {
        println!("Nothing to {what}.");
        return;
    }
    println!(
        "Freed {}. {} killed{}.",
        format_bytes(report.freed),
        plural(report.all_gone.len(), "process"),
        if report.survivors.is_empty() { String::new() } else { format!(", {} could not be ended", report.survivors.len()) }
    );
}

fn kill_all(cfg: &Config, yes: bool) -> io::Result<()> {
    let result = store::full_scan();
    let targets = result.killable_orphans(cfg);
    if targets.is_empty() {
        println!("Nothing left behind.");
        return Ok(());
    }
    let memory: u64 = targets.iter().map(|p| p.memory).sum();
    for p in &targets {
        println!("{}", proc_line(p, cfg));
    }
    if !yes && !confirm(&format!("Kill {} holding {}?", plural(targets.len(), "process"), format_bytes(memory))) {
        return Ok(());
    }
    let targets: Vec<Target> = targets.iter().map(Target::from).collect();
    report(&store::kill(&targets, cfg, Duration::from_secs(2)), "kill");
    Ok(())
}

fn kill_idle(cfg: &Config, yes: bool) -> io::Result<()> {
    let result = store::full_scan();
    let idle = result.idle_sessions(cfg);
    if idle.is_empty() {
        println!("No session has been idle longer than {}.", store::format_limit(cfg.idle_limit_minutes));
        return Ok(());
    }
    for s in &idle {
        let id = s.session_id.as_deref().unwrap_or("no-session");
        println!("  {}  pid {}", group_title(&s.project, s.title.as_deref(), id), s.process.pid);
    }
    let question = format!("End {}? Conversations can be picked up again with `claude --resume`.", plural(idle.len(), "session"));
    if !yes && !confirm(&question) {
        return Ok(());
    }
    let targets: Vec<Target> = idle
        .iter()
        .flat_map(|s| std::iter::once(&s.process).chain(s.procs.iter().filter(|p| !cfg.is_protected(&p.command))))
        .map(Target::from)
        .collect();
    report(&store::kill(&targets, cfg, Duration::from_secs(2)), "end");
    Ok(())
}

/// Open sessions' IDs and folders, from a fresh scan.
fn open_sessions() -> (std::collections::HashSet<String>, Vec<String>) {
    let result = store::full_scan();
    (
        result.sessions.iter().filter_map(|s| s.session_id.clone()).collect(),
        result.sessions.iter().map(|s| s.process.cwd.clone()).filter(|c| !c.is_empty()).collect(),
    )
}

fn storage(measure: bool) -> disk::StorageResult {
    let (open, cwds) = open_sessions();
    let mut result = disk::scan(&open, &cwds);
    if measure {
        if !result.worktrees.is_empty() {
            eprintln!("Measuring {}…", plural(result.worktrees.len(), "worktree"));
        }
        let measured: Vec<Option<disk::Measured>> = std::thread::scope(|scope| {
            let handles: Vec<_> = result.worktrees.iter().map(|wt| scope.spawn(move || disk::measure(wt))).collect();
            handles.into_iter().map(|h| h.join().ok()).collect()
        });
        for (wt, m) in result.worktrees.iter_mut().zip(measured) {
            if let Some(m) = m {
                wt.bytes = Some(m.bytes);
                wt.dirty = m.dirty;
                wt.only_here = m.only_here;
            }
        }
        result.worktrees.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    }
    result
}

fn disk_report(json: bool) -> io::Result<()> {
    let result = storage(true);
    if json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
        return Ok(());
    }
    println!("Worktrees: {} · {}", result.worktrees.len(), format_bytes(result.worktree_bytes()));
    for wt in &result.worktrees {
        let mut notes = Vec::new();
        notes.push(wt.branch.clone().unwrap_or_else(|| if wt.registered { "detached".into() } else { "unknown to git".into() }));
        if wt.dirty.unwrap_or(0) > 0 {
            notes.push(format!("{} uncommitted", wt.dirty.unwrap_or(0)));
        }
        if wt.only_here.unwrap_or(0) > 0 {
            notes.push(format!("{} commits on no branch", wt.only_here.unwrap_or(0)));
        }
        if !wt.exists {
            notes.push("folder gone".into());
        }
        println!(
            "  {:>9}  {} ⑂ {}  ({}, {})",
            wt.bytes.map(format_bytes).unwrap_or_default(),
            wt.repo_name(),
            wt.name(),
            notes.join(", "),
            plural(wt.sessions.len(), "session")
        );
    }
    println!("\nSession data: {} · {}", plural(result.sessions.len(), "session"), format_bytes(result.session_bytes()));
    for s in &result.sessions {
        println!(
            "  {:>9}  {:>4}  {}  {}{}{}",
            format_bytes(s.bytes),
            s.age.map(human_age).unwrap_or_default(),
            s.short_id(),
            s.project,
            s.title.as_deref().map(|t| format!(" — {t}")).unwrap_or_default(),
            if s.open { "  [open]" } else { "" }
        );
    }
    Ok(())
}

fn delete_sessions(ids: &[&str], yes: bool) -> io::Result<()> {
    if ids.is_empty() {
        return Err(io::Error::other("give the session IDs (or their first characters) to delete"));
    }
    let result = storage(false);
    let mut chosen = Vec::new();
    for id in ids {
        let id = id.to_lowercase();
        let hits: Vec<&disk::SessionData> = result.sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
        match hits.len() {
            0 => return Err(io::Error::other(format!("no session data for '{id}'"))),
            1 => chosen.push(hits[0].clone()),
            n => return Err(io::Error::other(format!("'{id}' matches {n} sessions; give more of the ID"))),
        }
    }
    for s in &chosen {
        println!("  {:>9}  {}  {}{}", format_bytes(s.bytes), s.id, s.project, s.title.as_deref().map(|t| format!(" — {t}")).unwrap_or_default());
        for p in &s.parts {
            println!("             {:>9}  {}", format_bytes(p.bytes), p.path.display());
        }
    }
    let question = "Move this to the Recycle Bin? Sessions without a transcript can no longer be resumed.";
    if !yes && !confirm(question) {
        return Ok(());
    }
    let (open, _) = open_sessions();
    let mut outcome = disk::Outcome::default();
    disk::delete_sessions(&chosen, &open, &mut outcome);
    for p in &outcome.problems {
        eprintln!("  {p}");
    }
    println!("Freed {}. {} moved to the Recycle Bin.", format_bytes(outcome.freed), plural(outcome.sessions_deleted, "session"));
    Ok(())
}

fn remove_worktree(which: &[&str], force: bool, yes: bool) -> io::Result<()> {
    let [which] = which else { return Err(io::Error::other("give one worktree: its path or its folder name")) };
    let result = storage(false);
    let want = which.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let hits: Vec<&disk::Worktree> =
        result.worktrees.iter().filter(|w| w.path.to_lowercase() == want || w.name().to_lowercase() == want).collect();
    let wt = match hits.as_slice() {
        [one] => (*one).clone(),
        [] => return Err(io::Error::other(format!("no worktree '{which}' in the repositories your sessions worked in"))),
        _ => return Err(io::Error::other(format!("'{which}' matches several worktrees; give the full path"))),
    };
    let mut wt = wt;
    let measured = disk::measure(&wt);
    wt.bytes = Some(measured.bytes);
    println!(
        "  {} ⑂ {}  {}  {}\n  {}",
        wt.repo_name(),
        wt.name(),
        format_bytes(measured.bytes),
        wt.branch.clone().unwrap_or_else(|| "detached".into()),
        wt.path
    );
    if measured.dirty.unwrap_or(0) > 0 || measured.only_here.unwrap_or(0) > 0 {
        println!(
            "  ⚠ {} uncommitted, {} commits on no branch{}",
            measured.dirty.unwrap_or(0),
            measured.only_here.unwrap_or(0),
            if force { ": --force removes them for good" } else { ": add --force to remove anyway" }
        );
    }
    if !yes && !confirm("Delete this worktree's folder for good? Its branch is kept.") {
        return Ok(());
    }
    let snap = scanner::Snapshot::take();
    let mut outcome = disk::Outcome::default();
    disk::remove_worktree(&wt, force, &snap, &mut outcome);
    for p in &outcome.problems {
        eprintln!("  {p}");
    }
    if outcome.worktrees_removed > 0 {
        println!("Removed. Freed {}.", format_bytes(outcome.freed));
    }
    Ok(())
}

/// Every theme as a row of color swatches, in true color.
fn show_themes(cfg: &Config) {
    use ratatui::style::Color;
    fn swatch(color: Color) -> String {
        match color {
            Color::Rgb(r, g, b) => format!("\x1b[48;2;{r};{g};{b}m   \x1b[0m"),
            _ => "   ".into(),
        }
    }
    fn fg(color: Color, text: &str) -> String {
        match color {
            Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m"),
            _ => text.into(),
        }
    }
    let current = themes::by_name(&cfg.theme).name;
    println!();
    for t in themes::THEMES {
        let mark = if t.name == current { fg(t.accent, "▸") } else { " ".into() };
        let colors: String = [t.bg.unwrap_or(Color::Reset), t.accent, t.info, t.good, t.warn, t.text, t.dim].into_iter().map(swatch).collect();
        let default = if std::ptr::eq(t, themes::DEFAULT) { " (default)" } else { "" };
        println!(" {mark} {} {colors}  {}{default}", fg(t.accent, &format!("{:<16}", t.name)), fg(t.dim, t.blurb));
    }
    println!("\n   Pick one with  hana --theme <name>,  or press t / T in the interactive view.\n");
}
