//! Checking GitHub for a newer release of hana, and swapping the running program for it.
//!
//! Downloads go through curl (curl.exe ships with Windows 10 1803 and later; Linux has curl), so hana
//! carries no HTTP or TLS code of its own. The only thing sent is a plain request for the latest
//! release; checks happen at most once a day and can be turned off in settings.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::store::Config;
use crate::os;

const REPO: &str = "cyber1998/hana";
const ASSET: &str = os::RELEASE_ASSET;
const SUMS: &str = "SHA256SUMS.txt";
const CHECK_EVERY_SECS: u64 = 24 * 3600;

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// "v1.2.3", "1.2", "1.2.3-beta" → (1, 2, 3). Pre-release and build suffixes are ignored.
pub fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    Some((major, minor, patch))
}

/// Whether `latest` is a newer version than `current`. Unreadable versions never are.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// Unix seconds of the last check that reached GitHub, or failed trying.
    checked: u64,
    /// Tag of the latest release, e.g. "v0.2.0".
    latest: String,
}

fn cache_path() -> PathBuf {
    Config::path().with_file_name("update.json")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// curl, as the system layer finds it.
fn curl() -> Command {
    let mut cmd = Command::new(os::curl_program());
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--max-time", "60", "--user-agent", "hana-updater"]);
    cmd.stdin(Stdio::null());
    cmd
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let out = curl().arg(url).output().map_err(|e| format!("could not run curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ").to_string())
    }
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let out = curl().arg("--output").arg(to).arg(url).output().map_err(|e| format!("could not run curl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ").to_string())
    }
}

/// The tag of the latest published release, straight from GitHub.
pub fn latest_tag() -> Result<String, String> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let body = fetch(&format!("https://api.github.com/repos/{REPO}/releases/latest"))?;
    let release: Release = serde_json::from_slice(&body).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    Ok(release.tag_name)
}

/// The tag of a newer release, if there is one. Asks GitHub at most once a day unless `force`;
/// otherwise answers from what the last check found. Never fails: no answer means no update.
pub fn check(force: bool) -> Option<String> {
    let path = cache_path();
    let cached: Cache = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let latest = if !force && now().saturating_sub(cached.checked) < CHECK_EVERY_SECS {
        cached.latest
    } else {
        // Offline or rate-limited: remember the attempt anyway, so hana does not ask on every start.
        let latest = latest_tag().unwrap_or(cached.latest);
        let cache = Cache { checked: now(), latest: latest.clone() };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(&path, serde_json::to_string_pretty(&cache).unwrap_or_default());
        latest
    };
    is_newer(&latest, current()).then_some(latest)
}

/// The checksum listed for `name` in a SHA256SUMS file ("<hex>  name" or "<hex> *name").
fn listed_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Downloads the release tagged `tag`, checks it against the release's checksums and that it runs,
/// then puts it in place of the running program. Windows lets a running exe be renamed but not
/// overwritten, so the old one moves aside to <name>.old and is deleted on the next start.
pub fn install(tag: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("could not find hana itself: {e}"))?;
    let new = sibling(&exe, "new");
    let old = sibling(&exe, "old");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");

    let sums = fetch(&format!("{base}/{SUMS}"))?;
    let expected = listed_sum(&String::from_utf8_lossy(&sums), ASSET).ok_or("the release lists no checksum for this system's build")?;
    let _ = fs::remove_file(&new);
    download(&format!("{base}/{ASSET}"), &new)?;
    let result = os::make_executable(&new)
        .map_err(|e| format!("could not make the download runnable: {e}"))
        .and_then(|()| verify(&new, &expected, tag))
        .and_then(|()| swap(&exe, &new, &old));
    let _ = fs::remove_file(&new);
    result
}

fn verify(file: &Path, expected: &str, tag: &str) -> Result<(), String> {
    let data = fs::read(file).map_err(|e| format!("could not read the download: {e}"))?;
    let actual = os::sha256(&data).map(|h| hex(&h)).ok_or("could not compute a checksum")?;
    if actual != expected {
        return Err("the download does not match the release's checksum; nothing was changed".into());
    }
    let out = Command::new(file).arg("--version").stdin(Stdio::null()).output().map_err(|e| format!("the new hana does not start: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout);
    match parse(said.trim().trim_start_matches("hana")) {
        Some(v) if Some(v) == parse(tag) => Ok(()),
        _ => Err(format!("the new hana reports \"{}\" instead of {tag}; nothing was changed", said.trim())),
    }
}

fn swap(exe: &Path, new: &Path, old: &Path) -> Result<(), String> {
    let _ = fs::remove_file(old);
    fs::rename(exe, old).map_err(|e| format!("could not move the current hana aside: {e}"))?;
    if let Err(e) = fs::rename(new, exe) {
        let _ = fs::rename(old, exe);
        return Err(format!("could not put the new hana in place: {e}"));
    }
    Ok(())
}

/// The program's own path with a suffix: hana.exe → hana.exe.new, hana → hana.new.
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(format!(".{suffix}"));
    PathBuf::from(name)
}

/// Removes the <name>.old an update left behind. On Windows it can't be deleted while it still runs,
/// so this happens on the next start.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(sibling(&exe, "old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert_eq!(parse("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse("0.2"), Some((0, 2, 0)));
        assert_eq!(parse("v1.0.0-beta.1"), Some((1, 0, 0)));
        assert_eq!(parse("latest"), None);
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    #[test]
    fn reads_checksum_files() {
        let sums = "ABC123 *hana.exe\ndef456  hana-v0.1.0-x86_64-windows.zip\n";
        assert_eq!(listed_sum(sums, "hana.exe").as_deref(), Some("abc123"));
        assert_eq!(listed_sum(sums, "hana-v0.1.0-x86_64-windows.zip").as_deref(), Some("def456"));
        assert_eq!(listed_sum(sums, "other.exe"), None);
    }

    #[test]
    fn hashes_like_sha256() {
        let empty = os::sha256(b"").map(|h| hex(&h));
        assert_eq!(empty.as_deref(), Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
        let abc = os::sha256(b"abc").map(|h| hex(&h));
        assert_eq!(abc.as_deref(), Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    }
}
