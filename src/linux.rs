//! The Linux side of what hana needs from the system, with the same functions as win.rs.
//!
//! Everything about a process is a file under /proc/<pid>: its environment, command line, folder,
//! start time and memory. Like on Windows, only processes of the same user can be read in full.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

// MARK: what differs between systems (win.rs has the same items)

pub const SEP: char = '/';
pub const CASE_INSENSITIVE: bool = false;
pub const TRASH_NAME: &str = "trash";
pub const RELEASE_ASSET: &str = "hana-linux-x86_64";

/// Process start times are clock ticks since boot, as /proc/<pid>/stat gives them.
pub fn ticks_per_second() -> u64 {
    static TICKS: OnceLock<u64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        // SAFETY: sysconf only reads a system constant.
        let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if t > 0 {
            t as u64
        } else {
            100
        }
    })
}

/// Linux paths already use '/'; a backslash is an ordinary character in a name.
pub fn native(path: &str) -> String {
    path.to_string()
}

/// When the system booted, in seconds since 1970.
fn boot_time() -> Option<u64> {
    static BOOT: OnceLock<Option<u64>> = OnceLock::new();
    *BOOT.get_or_init(|| fs::read_to_string("/proc/stat").ok()?.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()))
}

/// A start time as milliseconds since 1970, to compare with timestamps Claude Code records.
pub fn epoch_ms(created: u64) -> Option<f64> {
    let boot = boot_time()?;
    (created > 0).then(|| boot as f64 * 1000.0 + created as f64 * 1000.0 / ticks_per_second() as f64)
}

/// Where hana keeps its settings: $XDG_CONFIG_HOME, or the .config folder in the home folder.
pub fn config_base() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home().join(".config"),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into())
}

/// curl from PATH; nearly every distribution has it.
pub fn curl_program() -> PathBuf {
    PathBuf::from("curl")
}

/// A downloaded file needs the executable bit before it can run.
pub fn make_executable(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

pub fn open_folder(path: &str) -> io::Result<()> {
    Command::new("xdg-open").arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}

/// The first clipboard tool that is there: Wayland, X11, then Windows' own when running under WSL.
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    use std::io::Write;
    let tools: [(&str, &[&str]); 4] =
        [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"]), ("clip.exe", &[])];
    for (tool, args) in tools {
        let Ok(mut child) = Command::new(tool).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
            continue;
        };
        child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
        child.wait()?;
        return Ok(());
    }
    Err(io::Error::other("no clipboard tool found (install wl-clipboard or xclip)"))
}

// MARK: processes

pub fn current_pid() -> u32 {
    std::process::id()
}

/// Now, in clock ticks since boot: the unit start times are kept in.
pub fn now_ticks() -> u64 {
    let uptime = fs::read_to_string("/proc/uptime").ok().and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok()).unwrap_or(0.0);
    (uptime * ticks_per_second() as f64) as u64
}

pub struct Entry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

/// The fields of /proc/<pid>/stat after the command name, which may itself hold spaces and
/// parentheses. Index 0 is the state, 1 the parent, 19 the start time.
fn stat_fields(pid: u32) -> Option<(String, Vec<String>)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text[open + 1..close].to_string();
    Some((comm, text[close + 1..].split_whitespace().map(str::to_string).collect()))
}

fn args_of(pid: u32) -> Vec<String> {
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else { return Vec::new() };
    raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Every process, with its parent and program name.
pub fn list_processes() -> Vec<Entry> {
    let Ok(dir) = fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let (comm, fields) = stat_fields(pid)?;
            let ppid = fields.get(1)?.parse().unwrap_or(0);
            // The kernel cuts the name to 15 characters, so the program's own first argument names it better.
            let name = args_of(pid).first().map(|a| base_name(a).to_string()).filter(|n| !n.is_empty()).unwrap_or(comm);
            Some(Entry { pid, ppid, name })
        })
        .collect()
}

/// What hana can learn about one process. Fields stay empty for processes it may not read.
#[derive(Default)]
pub struct Inspected {
    /// Start time in clock ticks since boot; 0 if unknown.
    pub created: u64,
    pub path: String,
    pub cmdline: String,
    pub cwd: String,
    /// None when the environment could not be read.
    pub env: Option<HashMap<String, String>>,
    /// Private memory: what the process holds that is not shared with others.
    pub memory: u64,
}

fn start_time(pid: u32) -> Option<(u64, char)> {
    let (_, fields) = stat_fields(pid)?;
    let state = fields.first()?.chars().next()?;
    Some((fields.get(19)?.parse().ok()?, state))
}

/// One command line from the argument list, quoting arguments with spaces the way a shell would show them.
fn join_args(args: &[String]) -> String {
    args.iter().map(|a| if a.contains(char::is_whitespace) { format!("\"{a}\"") } else { a.clone() }).collect::<Vec<_>>().join(" ")
}

fn private_memory(pid: u32) -> u64 {
    let field = |text: &str, key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse::<u64>().ok());
    if let Ok(text) = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        if let (Some(clean), Some(dirty)) = (field(&text, "Private_Clean:"), field(&text, "Private_Dirty:")) {
            return (clean + dirty) * 1024;
        }
    }
    fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|t| field(&t, "RssAnon:")).map_or(0, |kb| kb * 1024)
}

pub fn inspect(pid: u32, want_env: bool) -> Inspected {
    let link = |what: &str| {
        fs::read_link(format!("/proc/{pid}/{what}")).map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_string()).unwrap_or_default()
    };
    let env = if want_env {
        fs::read(format!("/proc/{pid}/environ")).ok().map(|raw| {
            raw.split(|&b| b == 0)
                .filter_map(|kv| {
                    let kv = String::from_utf8_lossy(kv);
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect()
        })
    } else {
        None
    };
    Inspected {
        created: start_time(pid).map_or(0, |(t, _)| t),
        path: link("exe"),
        cmdline: join_args(&args_of(pid)),
        cwd: link("cwd"),
        env,
        memory: private_memory(pid),
    }
}

/// Listening TCP ports by PID: sockets from /proc/net/tcp{,6}, matched to the processes holding them.
pub fn listening_ports() -> HashMap<u32, Vec<u16>> {
    let mut by_inode: HashMap<u64, u16> = HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // State 0A is LISTEN.
            if cols.len() < 10 || cols[3] != "0A" {
                continue;
            }
            let Some(port) = cols[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok()) else { continue };
            if let Ok(inode) = cols[9].parse::<u64>() {
                by_inode.insert(inode, port);
            }
        }
    }
    let mut out: HashMap<u32, Vec<u16>> = HashMap::new();
    if by_inode.is_empty() {
        return out;
    }
    let Ok(dir) = fs::read_dir("/proc") else { return out };
    for pid in dir.flatten().filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok()) {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else { continue };
            let target = target.to_string_lossy();
            let Some(inode) = target.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')).and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            if let Some(&port) = by_inode.get(&inode) {
                let ports = out.entry(pid).or_default();
                if !ports.contains(&port) {
                    ports.push(port);
                }
            }
        }
    }
    out
}

/// Asks processes to end with SIGTERM, which is what closing them politely means on Linux.
/// Returns the PIDs that were asked.
pub fn request_close(targets: &[(u32, u64)]) -> Vec<u32> {
    targets.iter().filter(|&&(pid, created)| signal(pid, created, libc::SIGTERM)).map(|&(pid, _)| pid).collect()
}

/// True while the process with this PID is the one started at `created` and has not exited.
/// A zombie (exited, waiting for its parent) counts as gone.
pub fn is_running(pid: u32, created: u64) -> bool {
    match start_time(pid) {
        Some((started, state)) => (created == 0 || started == created) && state != 'Z' && state != 'X',
        None => false,
    }
}

/// Ends the process at once with SIGKILL.
pub fn terminate(pid: u32, created: u64) -> bool {
    signal(pid, created, libc::SIGKILL)
}

fn signal(pid: u32, created: u64, sig: i32) -> bool {
    if pid == 0 || !is_running(pid, created) {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: kill only sends a signal; the PID was checked to still be the same process just above.
    unsafe { libc::kill(pid, sig) == 0 }
}

// MARK: trash

/// Moves files and folders to the desktop trash (the freedesktop.org Trash specification), so a
/// deletion can be undone from any file manager. Fails rather than deleting for good when a path
/// is on another drive than the trash.
pub fn recycle(paths: &[PathBuf]) -> Result<(), String> {
    let trash = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home().join(".local").join("share"),
    }
    .join("Trash");
    let (files, info) = (trash.join("files"), trash.join("info"));
    fs::create_dir_all(&files).and_then(|()| fs::create_dir_all(&info)).map_err(|e| format!("could not open the trash: {e}"))?;
    for path in paths {
        let path = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "item".into());
        // A free name in the trash: name, name.2, name.3, …; claimed by creating its .trashinfo first.
        let mut n = 1;
        let (name, info_file) = loop {
            let name = if n == 1 { base.clone() } else { format!("{base}.{n}") };
            let info_file = info.join(format!("{name}.trashinfo"));
            match fs::OpenOptions::new().write(true).create_new(true).open(&info_file) {
                Ok(_) if !files.join(&name).exists() => break (name, info_file),
                Ok(_) => {
                    let _ = fs::remove_file(&info_file);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("could not write to the trash: {e}")),
            }
            n += 1;
        };
        let record = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", percent_encode(&path.to_string_lossy()), now_local_iso());
        if let Err(e) = fs::write(&info_file, record) {
            let _ = fs::remove_file(&info_file);
            return Err(format!("could not write to the trash: {e}"));
        }
        if let Err(e) = fs::rename(&path, files.join(&name)) {
            let _ = fs::remove_file(&info_file);
            return Err(format!("{}: could not move it to the trash ({e})", path.display()));
        }
    }
    Ok(())
}

fn percent_encode(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The time now as YYYY-MM-DDThh:mm:ss, in UTC (close enough for the trash's own records).
fn now_local_iso() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}", rest / 3600, rest / 60 % 60, rest % 60)
}

// MARK: SHA-256

/// SHA-256 of `data` (FIPS 180-4). Small and only used to check downloads.
pub fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_itself() {
        let me = inspect(current_pid(), true);
        assert!(me.created > 0);
        assert!(me.env.is_some_and(|e| e.contains_key("PATH") || e.contains_key("HOME")));
        assert!(is_running(current_pid(), me.created));
        assert!(!is_running(current_pid(), me.created + 1));
        assert!(list_processes().iter().any(|e| e.pid == current_pid()));
    }

    #[test]
    fn sees_its_own_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ports = listening_ports();
        assert!(ports.get(&current_pid()).is_some_and(|p| p.contains(&port)), "port {port} not found: {:?}", ports.get(&current_pid()));
    }

    /// A file and a folder go to the trash with their .trashinfo, and a second file of the same
    /// name gets a name of its own. Uses a scratch XDG_DATA_HOME, never the real trash.
    #[test]
    fn moves_to_trash() {
        let base = std::env::temp_dir().join(format!("hana-trash-test-{}", current_pid()));
        let _ = fs::remove_dir_all(&base);
        let data = base.join("data");
        let work = base.join("work");
        fs::create_dir_all(work.join("folder")).unwrap();
        fs::write(work.join("a.txt"), "a").unwrap();
        fs::write(work.join("folder").join("b.txt"), "b").unwrap();
        // SAFETY: no other test reads XDG_DATA_HOME.
        unsafe { std::env::set_var("XDG_DATA_HOME", &data) };

        recycle(&[work.join("a.txt"), work.join("folder")]).unwrap();
        fs::write(work.join("a.txt"), "again").unwrap();
        recycle(&[work.join("a.txt")]).unwrap();

        let trash = data.join("Trash");
        assert!(!work.join("a.txt").exists() && !work.join("folder").exists());
        assert_eq!(fs::read_to_string(trash.join("files").join("a.txt")).unwrap(), "a");
        assert_eq!(fs::read_to_string(trash.join("files").join("a.txt.2")).unwrap(), "again");
        assert!(trash.join("files").join("folder").join("b.txt").is_file());
        let info = fs::read_to_string(trash.join("info").join("a.txt.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/") && info.contains("/work/a.txt\n") && info.contains("DeletionDate=20"), "{info}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn trash_paths_and_dates() {
        let path = std::env::temp_dir().join("a b").join("ü.txt");
        let encoded = percent_encode(&path.to_string_lossy());
        assert!(encoded.starts_with('/') && encoded.ends_with("/a%20b/%C3%BC.txt"), "{encoded}");
        let date = now_local_iso();
        assert_eq!(date.len(), 19);
        assert!(date.starts_with("20"));
    }
}
