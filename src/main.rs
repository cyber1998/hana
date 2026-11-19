//! seshka for Windows: kill what Claude Code left behind.

mod scanner;
mod store;
mod tui;
mod win;

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

use store::{format_bytes, human_age, plural, Config, Target};

const HELP: &str = "\
seshka - kill what Claude Code left behind (Windows)

Usage:
  seshka                 Open the interactive view
  seshka list [--json]   Print leftovers and open sessions, then exit
  seshka kill-all [-y]   Kill every leftover process (except protected ones)
  seshka kill-idle [-y]  End sessions idle longer than the limit (see settings)
  seshka config          Print where settings are kept, and what they are
  seshka --help | --version

Settings live in %APPDATA%\\seshka\\config.json and can be changed from the interactive view (s).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let yes = args.iter().any(|a| a == "-y" || a == "--yes");
    let json = args.iter().any(|a| a == "--json");
    let command = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
    let mut cfg = Config::load();

    let result = match command {
        _ if args.iter().any(|a| a == "-h" || a == "--help") => {
            print!("{HELP}");
            Ok(())
        }
        _ if args.iter().any(|a| a == "-V" || a == "--version") => {
            println!("seshka {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None => tui::run(&mut cfg),
        Some("list") => list(&cfg, json),
        Some("kill-all") => kill_all(&cfg, yes),
        Some("kill-idle") => kill_idle(&cfg, yes),
        Some("config") => {
            println!("{}", Config::path().display());
            println!("{}", serde_json::to_string_pretty(&cfg).unwrap_or_default());
            Ok(())
        }
        Some(other) => {
            eprintln!("seshka: unknown command '{other}'\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("seshka: {e}");
            ExitCode::FAILURE
        }
    }
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
