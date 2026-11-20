//! The README's screenshots. The real drawing code renders made-up sessions into an off-screen
//! terminal, and each frame is written out as an SVG that looks like a terminal window.
//!
//! Regenerate with:  cargo test --release readme_screens -- --ignored
//!
//! Every name, path and number here is invented, so the pictures never show anyone's projects.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use super::*;
use crate::disk::{SessionData, StorageResult, Worktree};
use crate::scanner::{Helper, Proc, RunningSession, SessionGroup, Status};
use crate::store::{Config, ScanResult};

const COLS: u16 = 104;


const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * MB;

fn proc(pid: u32, session: &str, alive: bool, command: &str, cwd: &str, ports: &[u16], memory: u64, age: u64, helpers: usize) -> Proc {
    Proc {
        pid,
        ppid: 0,
        created: 1_000 + pid as u64,
        age,
        session_id: session.into(),
        session_alive: alive,
        command: command.into(),
        // The executable: the quoted first argument, or everything up to the first space.
        path: match command.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or_default().into(),
            None => command.split(' ').next().unwrap_or_default().into(),
        },
        cwd: cwd.into(),
        ports: ports.to_vec(),
        memory,
        helpers: (0..helpers).map(|i| Helper { pid: pid + 1 + i as u32, created: 0 }).collect(),
    }
}

fn scan_result() -> ScanResult {
    let shop = "2f6c1d0e-5b7a-4c3e-9f21-8d4e6a1b7c90";
    let api = "9a8b7c6d-1e2f-4a3b-8c5d-6e7f8a9b0c1d";
    let docs = "4d3c2b1a-0f9e-4d8c-a7b6-5c4d3e2f1a0b";
    let game = "7e6f5a4b-3c2d-4e1f-b0a9-8c7d6e5f4a3b";
    let now = scanner::now_ms();
    ScanResult {
        orphans: vec![
            SessionGroup {
                id: shop.into(),
                project: "webshop".into(),
                title: Some("Checkout redesign".into()),
                procs: vec![
                    proc(41208, shop, false, r"node.exe node_modules\vite\bin\vite.js --port 5173", "webshop", &[5173], 182 * MB, 3 * 3600 + 120, 1),
                    proc(41330, shop, false, r"node.exe node_modules\typescript\bin\tsc --watch", "webshop", &[], 96 * MB, 3 * 3600 + 100, 1),
                    proc(
                        41512,
                        shop,
                        false,
                        "chrome.exe --remote-debugging-port=9222 --headless",
                        "webshop",
                        &[9222],
                        612 * MB,
                        2 * 3600 + 3000,
                        11,
                    ),
                ],
            },
            SessionGroup {
                id: api.into(),
                project: "api-server".into(),
                title: Some("Rate limiter".into()),
                procs: vec![proc(
                    38876,
                    api,
                    false,
                    "python.exe -m uvicorn app.main:app --reload --port 8000",
                    "api-server",
                    &[8000],
                    74 * MB,
                    26 * 3600,
                    1,
                )],
            },
        ],
        sessions: vec![
            RunningSession {
                process: proc(30112, docs, true, "claude.exe", "docs-site", &[], 418 * MB, 47 * 60, 0),
                session_id: Some(docs.into()),
                project: "docs-site".into(),
                title: Some("Search page".into()),
                status: Status::Busy,
                status_since_ms: Some(now - 20_000.0),
                procs: vec![proc(30290, docs, true, r"node.exe node_modules\astro\astro.js dev", "docs-site", &[4321], 133 * MB, 40 * 60, 1)],
            },
            RunningSession {
                process: proc(27764, game, true, "claude.exe", r"game\.claude\worktrees\physics-2a1f9c", &[], 356 * MB, 5 * 3600, 0),
                session_id: Some(game.into()),
                project: "game ⑂ physics-2a1f9c".into(),
                title: Some("Physics tuning".into()),
                status: Status::Idle,
                status_since_ms: Some(now - 25.0 * 60_000.0),
                procs: vec![],
            },
        ],
        live: vec![],
    }
}

fn storage_result() -> StorageResult {
    let data = |id: &str, project: &str, title: &str, bytes: u64, age_days: u64, open: bool| SessionData {
        id: id.into(),
        project: project.into(),
        title: Some(title.into()),
        cwd: Some(project.split(' ').next().unwrap_or(project).to_string()),
        age: Some(age_days * 86_400),
        parts: vec![],
        bytes,
        open,
    };
    StorageResult {
        sessions: vec![
            data("4d3c2b1a-0f9e-4d8c-a7b6-5c4d3e2f1a0b", "docs-site", "Search page", 212 * MB, 0, true),
            data("a1b2c3d4-0000-4000-8000-000000000001", "webshop", "Checkout redesign", 164 * MB, 2, false),
            data("a1b2c3d4-0000-4000-8000-000000000002", "game ⑂ renderer-77c0d1", "Shadow cascades", 131 * MB, 41, false),
            data("a1b2c3d4-0000-4000-8000-000000000003", "api-server", "Rate limiter", 88 * MB, 1, false),
            data("a1b2c3d4-0000-4000-8000-000000000004", "game ⑂ audio-5e2b90", "Footstep sounds", 76 * MB, 38, false),
            data("a1b2c3d4-0000-4000-8000-000000000005", "webshop", "Fix flaky cart test", 41 * MB, 9, false),
            data("a1b2c3d4-0000-4000-8000-000000000006", "dotfiles", "PowerShell prompt", 12 * MB, 63, false),
            data("a1b2c3d4-0000-4000-8000-000000000007", "api-server", "OpenAPI docs", 9 * MB, 17, false),
        ],
        worktrees: vec![
            Worktree {
                repo: "game".into(),
                path: r"game\.claude\worktrees\renderer-77c0d1".into(),
                branch: None,
                head: "c0ffee1a2b3c4d5e6f".into(),
                registered: true,
                exists: true,
                locked: false,
                bytes: Some(6 * GB + 300 * MB),
                dirty: Some(0),
                only_here: Some(0),
                sessions: vec!["a1b2c3d4-0000-4000-8000-000000000002".into()],
            },
            Worktree {
                repo: "webshop".into(),
                path: r"webshop\.claude\worktrees\checkout-91be04".into(),
                branch: Some("checkout-redesign".into()),
                head: "badc0de1234567".into(),
                registered: true,
                exists: true,
                locked: false,
                bytes: Some(1 * GB + 200 * MB),
                dirty: Some(3),
                only_here: Some(0),
                sessions: vec!["a1b2c3d4-0000-4000-8000-000000000001".into()],
            },
        ],
    }
}

fn app(theme: &str) -> App {
    let cfg = Config { theme: theme.into(), check_for_updates: false, ..Config::default() };
    let mut app = App::new(cfg);
    app.result = Some(scan_result());
    app.storage = Some(storage_result());
    app
}

fn render(app: &mut App, rows: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(COLS, rows)).expect("test terminal");
    terminal.draw(|f| app.draw(f)).expect("draw");
    terminal.backend().buffer().clone()
}

// MARK: SVG

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
const PAD: f32 = 14.0;
const BAR: f32 = 30.0;

fn hex(color: Color, fallback: &str) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02X}{g:02X}{b:02X}"),
        Color::Reset => fallback.to_string(),
        Color::Black => "#1C1C1C".into(),
        Color::Red | Color::LightRed => "#E06C75".into(),
        Color::Green | Color::LightGreen => "#98C379".into(),
        Color::Yellow | Color::LightYellow => "#E5C07B".into(),
        Color::Blue | Color::LightBlue => "#61AFEF".into(),
        Color::Magenta | Color::LightMagenta => "#C678DD".into(),
        Color::Cyan | Color::LightCyan => "#56B6C2".into(),
        Color::Gray => "#ABB2BF".into(),
        Color::DarkGray => "#5C6370".into(),
        Color::White => "#E6E6E6".into(),
        _ => fallback.to_string(),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The buffer as an SVG terminal window. Text is laid out run by run, each run stretched to its
/// exact cell width so columns line up whatever font the viewer has.
fn to_svg(buf: &Buffer, theme: &Theme, title: &str) -> String {
    let (default_bg, default_fg) = match theme.bg {
        Some(bg) => (hex(bg, "#1C1C1C"), hex(theme.text, "#E6E6E6")),
        None => ("#1C1C1C".to_string(), "#E6E6E6".to_string()),
    };
    let rows = buf.area.height;
    let width = COLS as f32 * CELL_W + PAD * 2.0;
    let height = rows as f32 * CELL_H + PAD * 2.0 + BAR;
    let right = width - 1.0;
    let mut out = String::new();
    // Windows Terminal's frame: a tab strip with the open tab, + and the dropdown, then the
    // minimize, maximize and close buttons on the right.
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="{title}">
<style>text{{font-family:"Cascadia Mono","Cascadia Code",Consolas,ui-monospace,Menlo,monospace;font-size:14px;white-space:pre}} .ui{{font-family:"Segoe UI Variable Text","Segoe UI",system-ui,sans-serif;font-size:12px}}</style>
<rect width="{width}" height="{height}" rx="8" fill="#202020"/>
<rect x="0.5" y="0.5" width="{w1}" height="{h1}" rx="8" fill="none" stroke="#3a3a3a"/>
<path d="M8 {BAR} V12 a6 6 0 0 1 6 -6 H226 a6 6 0 0 1 6 6 V{BAR} Z" fill="{default_bg}"/>
<g fill="none" stroke="{default_fg}" stroke-opacity="0.75" stroke-width="1.2" stroke-linecap="round"><path d="M20 15 l3.5 3 l-3.5 3"/><path d="M25.5 21.5 h5"/></g>
<text class="ui" x="40" y="22" fill="{default_fg}">{title}</text>
<g stroke="{default_fg}" stroke-opacity="0.6" stroke-width="1.1" fill="none"><path d="M212 14.5 l6 6 M218 14.5 l-6 6"/><path d="M248 18 h10 M253 13 v10"/><path d="M270 16.5 l3.5 3.5 l3.5 -3.5"/></g>
<g stroke="{default_fg}" stroke-opacity="0.8" stroke-width="1" fill="none"><path d="M{min_x} 18.5 h10"/><rect x="{max_x}" y="13.5" width="9" height="9" rx="1.5"/><path d="M{close_x} 13.5 l9 9 M{close_x2} 13.5 l-9 9"/></g>
<rect y="{BAR}" width="{width}" height="{body_h}" fill="{default_bg}"/>
<path d="M0 {body_bottom} H{width} V{rounded} a8 8 0 0 1 -8 8 H8 a8 8 0 0 1 -8 -8 Z" fill="{default_bg}"/>
"##,
        title = escape(title),
        w1 = width - 1.0,
        h1 = height - 1.0,
        min_x = right - 140.0,
        max_x = right - 94.5,
        close_x = right - 39.5,
        close_x2 = right - 30.5,
        body_h = height - BAR - 8.0,
        body_bottom = height - 8.0,
        rounded = height - 8.0,
    );

    for y in 0..rows {
        let top = BAR + PAD + y as f32 * CELL_H;
        // Backgrounds first, merged into runs.
        let mut x = 0;
        while x < COLS {
            let bg = buf[(x, y)].bg;
            let start = x;
            while x < COLS && buf[(x, y)].bg == bg {
                x += 1;
            }
            let fill = hex(bg, &default_bg);
            if fill != default_bg {
                let _ = writeln!(
                    out,
                    r#"<rect x="{:.1}" y="{top:.1}" width="{:.1}" height="{CELL_H}" fill="{fill}"/>"#,
                    PAD + start as f32 * CELL_W,
                    (x - start) as f32 * CELL_W
                );
            }
        }
        // Half blocks are pixels: drawn as solid rectangles, the way a terminal shows them, rather
        // than as stretched glyphs.
        for x in 0..COLS {
            let cell = &buf[(x, y)];
            let top_half = match cell.symbol() {
                "▀" => true,
                "▄" => false,
                _ => continue,
            };
            let _ = writeln!(
                out,
                r#"<rect x="{:.1}" y="{:.1}" width="{CELL_W:.1}" height="{:.1}" fill="{}"/>"#,
                PAD + x as f32 * CELL_W,
                if top_half { top } else { top + CELL_H / 2.0 },
                CELL_H / 2.0,
                hex(cell.fg, &default_fg),
            );
        }
        // Then text, in runs of one style.
        let mut x = 0;
        while x < COLS {
            let cell = &buf[(x, y)];
            let (fg, modifier) = (cell.fg, cell.modifier);
            let start = x;
            let mut text = String::new();
            while x < COLS && buf[(x, y)].fg == fg && buf[(x, y)].modifier == modifier {
                // A wide character's second cell is empty; the first one already holds it.
                let symbol = buf[(x, y)].symbol();
                // A wide character (花) gets a text element of its own, two cells wide, so the
                // stretching below does not spread it across its neighbours.
                let wide = unicode_width::UnicodeWidthStr::width(symbol) == 2;
                if wide && x > start {
                    break;
                }
                text.push_str(if matches!(symbol, "▀" | "▄") { " " } else { symbol });
                x += 1;
                if wide {
                    x += 1;
                    break;
                }
            }
            if text.trim().is_empty() {
                continue;
            }
            let mut style = String::new();
            if modifier.contains(Modifier::BOLD) {
                style.push_str("font-weight:700;");
            }
            if modifier.contains(Modifier::ITALIC) {
                style.push_str("font-style:italic;");
            }
            let opacity = if modifier.contains(Modifier::DIM) { r#" fill-opacity="0.55""# } else { "" };
            let _ = writeln!(
                out,
                r#"<text x="{:.1}" y="{:.1}" textLength="{:.1}" lengthAdjust="spacingAndGlyphs" fill="{}"{opacity} style="{style}">{}</text>"#,
                PAD + start as f32 * CELL_W,
                top + 13.5,
                (x - start) as f32 * CELL_W,
                hex(fg, &default_fg),
                escape(&text),
            );
        }
    }
    out.push_str("</svg>\n");
    out
}

fn save(name: &str, svg: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join("screens");
    std::fs::create_dir_all(&dir).expect("docs/screens");
    std::fs::write(dir.join(name), svg).expect("write screen");
}

#[test]
#[ignore = "writes docs/screens; run on purpose to refresh the README's pictures"]
fn readme_screens() {
    // Processes: leftovers grouped by session, open sessions working and idle, a process selected.
    let mut a = app("cherry-blossom");
    a.toggled.insert("og:9a8b7c6d-1e2f-4a3b-8c5d-6e7f8a9b0c1d".into());
    a.selected_key = Some("op:41208:42208".into());
    let buf = render(&mut a, 18);
    save("processes.svg", &to_svg(&buf, a.theme(), "hana · processes"));

    // Storage: worktrees and session data, two old sessions marked for deletion.
    let mut a = app("cherry-blossom");
    a.view = View::Storage;
    a.marked.insert("sd:a1b2c3d4-0000-4000-8000-000000000002".into());
    a.marked.insert("sd:a1b2c3d4-0000-4000-8000-000000000004".into());
    a.selected_key = Some("sd:a1b2c3d4-0000-4000-8000-000000000004".into());
    let buf = render(&mut a, 21);
    save("storage.svg", &to_svg(&buf, a.theme(), "hana · storage"));

    // A cleanup in kawaii: what was freed, and the flower in full bloom.
    let mut a = app("kawaii");
    if let Some(r) = &mut a.result {
        r.orphans.clear();
    }
    a.freed = Some((965 * MB, "5 processes bonked".into(), Instant::now()));
    a.bloom = Instant::now().checked_sub(Duration::from_millis(1500));
    let buf = render(&mut a, 18);
    save("cleanup.svg", &to_svg(&buf, a.theme(), "hana · after a cleanup"));

    // The intro, once the flower is open.
    for (name, ms) in [("intro.svg", 1300)] {
        let t = crate::themes::by_name("cherry-blossom");
        let mut terminal = Terminal::new(TestBackend::new(COLS, 22)).expect("test terminal");
        terminal
            .draw(|f| {
                f.render_widget(Block::new().style(t.base()), f.area());
                let area = f.area();
                crate::intro::draw(f.buffer_mut(), area, Duration::from_millis(ms), t);
            })
            .expect("draw");
        save(name, &to_svg(terminal.backend().buffer(), t, "hana"));
    }
}
