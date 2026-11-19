//! The Win32 calls hana needs, wrapped so the rest of the program stays safe code.
//!
//! On macOS the original seshka reads everything through `ps` and `lsof`. Windows has no equivalent that shows
//! another process's environment, so it is read straight out of the process's PEB here.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_TIMEOUT};
use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE};

const SYNCHRONIZE: u32 = 0x0010_0000;
const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(handle: HANDLE, class: u32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(pid: u32, access: u32) -> Option<Handle> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        None
    } else {
        Some(Handle(handle))
    }
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn filetime(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// Now, in 100-nanosecond ticks since 1601, the unit process creation times are kept in.
pub fn now_filetime() -> u64 {
    let mut ft: FILETIME = unsafe { zeroed() };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    filetime(ft)
}

pub struct Entry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

/// Every process on the system, with its parent and executable name.
pub fn list_processes() -> Vec<Entry> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return out;
        }
        let _guard = Handle(snap);
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                out.push(Entry {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
                    name: from_wide(&entry.szExeFile),
                });
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
    }
    out
}

/// What hana can learn about one process. Fields stay empty when Windows does not allow reading them
/// (another user's processes, elevated ones, protected ones).
#[derive(Default)]
pub struct Inspected {
    /// Creation time as a FILETIME; 0 if unknown.
    pub created: u64,
    pub path: String,
    pub cmdline: String,
    pub cwd: String,
    /// None when the environment could not be read.
    pub env: Option<HashMap<String, String>>,
    /// Private bytes: memory the process holds that is not shared with others.
    pub memory: u64,
}

pub fn inspect(pid: u32, want_env: bool) -> Inspected {
    let mut out = Inspected::default();
    let handle = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ) {
        Some(h) => Some((h, true)),
        None => open(pid, PROCESS_QUERY_LIMITED_INFORMATION).map(|h| (h, false)),
    };
    let Some((handle, readable)) = handle else { return out };
    out.created = creation_time(&handle);
    out.path = image_path(&handle);
    out.memory = private_bytes(&handle);
    if readable {
        unsafe { read_peb(&handle, &mut out, want_env) };
    }
    out
}

fn creation_time(handle: &Handle) -> u64 {
    unsafe {
        let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
            (zeroed(), zeroed(), zeroed(), zeroed());
        if GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
            return 0;
        }
        filetime(created)
    }
}

fn image_path(handle: &Handle) -> String {
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, buf.as_mut_ptr(), &mut len) } == 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

fn private_bytes(handle: &Handle) -> u64 {
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = K32GetProcessMemoryInfo(
            handle.0,
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        );
        if ok == 0 {
            0
        } else {
            counters.PrivateUsage as u64
        }
    }
}

unsafe fn read<T: Copy>(handle: &Handle, addr: usize) -> Option<T> {
    let mut value: T = zeroed();
    let mut got = 0usize;
    let ok = ReadProcessMemory(handle.0, addr as *const c_void, &mut value as *mut T as *mut c_void, size_of::<T>(), &mut got);
    (ok != 0 && got == size_of::<T>()).then_some(value)
}

unsafe fn read_bytes(handle: &Handle, addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    let ok = ReadProcessMemory(handle.0, addr as *const c_void, buf.as_mut_ptr() as *mut c_void, len, &mut got);
    if ok == 0 && got == 0 {
        return None;
    }
    buf.truncate(got);
    Some(buf)
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

/// A UNICODE_STRING in a 64-bit process: Length (u16), MaximumLength (u16), padding, then the buffer pointer.
unsafe fn read_unicode_string(handle: &Handle, addr: usize) -> Option<String> {
    let len: u16 = read(handle, addr)?;
    let buffer: usize = read(handle, addr + 8)?;
    if len == 0 || buffer == 0 {
        return Some(String::new());
    }
    let bytes = read_bytes(handle, buffer, len as usize)?;
    Some(String::from_utf16_lossy(&utf16(&bytes)))
}

// Offsets into the 64-bit PEB and RTL_USER_PROCESS_PARAMETERS. These have been stable since Vista.
const PEB_PROCESS_PARAMETERS: usize = 0x20;
const PARAMS_CURRENT_DIRECTORY: usize = 0x38;
const PARAMS_COMMAND_LINE: usize = 0x70;
const PARAMS_ENVIRONMENT: usize = 0x80;
const PARAMS_ENVIRONMENT_SIZE: usize = 0x3F0;

unsafe fn read_peb(handle: &Handle, out: &mut Inspected, want_env: bool) {
    // PROCESS_BASIC_INFORMATION: ExitStatus, PebBaseAddress, AffinityMask, BasePriority, Pid, ParentPid.
    let mut info = [0usize; 6];
    let mut returned = 0u32;
    let status = NtQueryInformationProcess(handle.0, 0, info.as_mut_ptr() as *mut c_void, size_of::<[usize; 6]>() as u32, &mut returned);
    if status != 0 || info[1] == 0 {
        return;
    }
    let Some(params) = read::<usize>(handle, info[1] + PEB_PROCESS_PARAMETERS) else { return };
    if params == 0 {
        return;
    }
    if let Some(cwd) = read_unicode_string(handle, params + PARAMS_CURRENT_DIRECTORY) {
        let trimmed = cwd.trim_end_matches('\\');
        // Keep the backslash of a drive root, "C:\".
        out.cwd = if trimmed.ends_with(':') { cwd } else { trimmed.to_string() };
    }
    if let Some(cmd) = read_unicode_string(handle, params + PARAMS_COMMAND_LINE) {
        out.cmdline = cmd;
    }
    if !want_env {
        return;
    }
    let Some(env_ptr) = read::<usize>(handle, params + PARAMS_ENVIRONMENT) else { return };
    if env_ptr == 0 {
        return;
    }
    let size = read::<usize>(handle, params + PARAMS_ENVIRONMENT_SIZE).unwrap_or(0);
    let bytes = if size > 0 && size <= 16 * 1024 * 1024 {
        read_bytes(handle, env_ptr, size)
    } else {
        read_env_until_end(handle, env_ptr)
    };
    if let Some(bytes) = bytes {
        out.env = Some(parse_env(&utf16(&bytes)));
    }
}

/// When the block's size is unknown, read page by page until the double NUL that ends it.
unsafe fn read_env_until_end(handle: &Handle, addr: usize) -> Option<Vec<u8>> {
    let mut all = Vec::new();
    let mut at = addr;
    while all.len() < 4 * 1024 * 1024 {
        let to_page_end = 4096 - (at % 4096);
        let chunk = read_bytes(handle, at, to_page_end)?;
        if chunk.is_empty() {
            break;
        }
        all.extend_from_slice(&chunk);
        at += chunk.len();
        if all.windows(4).step_by(2).any(|w| w == [0, 0, 0, 0]) {
            break;
        }
    }
    Some(all)
}

/// An environment block: `NAME=value\0NAME=value\0\0`. Variable names are case-insensitive on Windows.
fn parse_env(block: &[u16]) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for entry in block.split(|&c| c == 0) {
        if entry.is_empty() {
            // An empty string marks the end of the block.
            break;
        }
        let text = String::from_utf16_lossy(entry);
        // Names may start with "=" (e.g. "=C:=C:\"), so look for the separator after the first character.
        if let Some(eq) = text.char_indices().skip(1).find(|&(_, c)| c == '=').map(|(i, _)| i) {
            env.insert(text[..eq].to_ascii_uppercase(), text[eq + 1..].to_string());
        }
    }
    env
}

/// TCP ports each process listens on, IPv4 and IPv6.
pub fn listening_ports() -> HashMap<u32, Vec<u16>> {
    let mut out: HashMap<u32, Vec<u16>> = HashMap::new();
    for (family, row_size, port_at, pid_at) in [(AF_INET, 24usize, 8usize, 20usize), (AF_INET6, 56, 20, 52)] {
        let Some(table) = tcp_table(family) else { continue };
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(table.as_ptr() as *const u8, table.len() * 4) };
        let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        for i in 0..count {
            let row = 4 + i * row_size;
            if row + row_size > bytes.len() {
                break;
            }
            let raw = u32::from_le_bytes(bytes[row + port_at..row + port_at + 4].try_into().unwrap());
            // The port is kept in network byte order in the low 16 bits.
            let port = (((raw & 0xff) << 8) | ((raw >> 8) & 0xff)) as u16;
            let pid = u32::from_le_bytes(bytes[row + pid_at..row + pid_at + 4].try_into().unwrap());
            let ports = out.entry(pid).or_default();
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    for ports in out.values_mut() {
        ports.sort_unstable();
    }
    out
}

fn tcp_table(family: u32) -> Option<Vec<u32>> {
    let mut size = 0u32;
    unsafe { GetExtendedTcpTable(null_mut(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0) };
    for _ in 0..4 {
        // Room for a few more rows, in case connections open between the two calls.
        let mut buf = vec![0u32; (size as usize + 1024) / 4];
        let mut len = (buf.len() * 4) as u32;
        let rc = unsafe { GetExtendedTcpTable(buf.as_mut_ptr() as *mut c_void, &mut len, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0) };
        if rc == 0 {
            return Some(buf);
        }
        if rc != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        size = len;
    }
    None
}

/// Visible top-level windows, by the process that owns them.
fn visible_windows() -> HashMap<u32, Vec<isize>> {
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let map = &mut *(lparam as *mut HashMap<u32, Vec<isize>>);
        if IsWindowVisible(hwnd) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            map.entry(pid).or_default().push(hwnd as isize);
        }
        1
    }
    let mut map: HashMap<u32, Vec<isize>> = HashMap::new();
    unsafe { EnumWindows(Some(each), &mut map as *mut _ as LPARAM) };
    map
}

/// Asks windowed apps to close, the way clicking their close button would.
/// Returns the PIDs that had a window to ask; console programs have none.
pub fn request_close(pids: &[u32]) -> Vec<u32> {
    let windows = visible_windows();
    let mut asked = Vec::new();
    for &pid in pids {
        if let Some(hwnds) = windows.get(&pid) {
            for &hwnd in hwnds {
                unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
            }
            asked.push(pid);
        }
    }
    asked
}

/// True while the process with this PID is the one created at `created` and has not exited.
/// PIDs get reused quickly on Windows, so the creation time is what identifies a process.
pub fn is_running(pid: u32, created: u64) -> bool {
    let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE) else { return false };
    if created != 0 && creation_time(&handle) != created {
        return false;
    }
    unsafe { WaitForSingleObject(handle.0, 0) == WAIT_TIMEOUT }
}

/// Ends the process at once. Returns false if it was gone, had been replaced, or could not be ended.
pub fn terminate(pid: u32, created: u64) -> bool {
    let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE) else { return false };
    if created != 0 && creation_time(&handle) != created {
        return false;
    }
    unsafe { TerminateProcess(handle.0, 1) != 0 }
}

/// Moves files and folders to the Recycle Bin, so a deletion can be undone from there.
/// Windows asks first if something is too big for the bin and would have to be deleted for good.
pub fn recycle(paths: &[std::path::PathBuf]) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW,
    };
    if paths.is_empty() {
        return Ok(());
    }
    // A list of NUL-terminated paths, ended by one more NUL.
    let mut from: Vec<u16> = Vec::new();
    for path in paths {
        if !path.is_absolute() {
            return Err(format!("not an absolute path: {}", path.display()));
        }
        from.extend(path.as_os_str().encode_wide());
        from.push(0);
    }
    from.push(0);
    unsafe {
        let mut op: SHFILEOPSTRUCTW = zeroed();
        op.wFunc = FO_DELETE as _;
        op.pFrom = from.as_ptr();
        op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT | FOF_WANTNUKEWARNING) as _;
        let rc = SHFileOperationW(&mut op);
        if rc != 0 {
            return Err(format!("the Recycle Bin refused it (error {rc:#x})"));
        }
        if op.fAnyOperationsAborted != 0 {
            return Err("cancelled".into());
        }
    }
    Ok(())
}

/// SHA-256 of `data`, computed by Windows (CNG).
pub fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let len = u32::try_from(data.len()).ok()?;
    // SAFETY: the algorithm handle is a predefined pseudo-handle; both buffers are valid for their lengths.
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, data.as_ptr(), len, out.as_mut_ptr(), 32) };
    (status == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycles_a_file() {
        let dir = std::env::temp_dir().join(format!("hana-recycle-test-{}", current_pid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        recycle(&[dir.clone()]).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn reads_own_environment() {
        std::env::set_var("HANA_TEST_MARKER", "yes");
        // The PEB copy is what child processes see; our own is updated by set_var too.
        let me = inspect(current_pid(), true);
        let env = me.env.expect("own environment is readable");
        assert_eq!(env.get("HANA_TEST_MARKER").map(String::as_str), Some("yes"));
        assert!(me.created > 0);
        assert!(me.cmdline.to_lowercase().contains("hana"));
    }

    #[test]
    fn parses_env_block() {
        let text = "=C:=C:\\\0PATH=a;b\0Claudecode=1\0\0";
        let block: Vec<u16> = text.encode_utf16().collect();
        let env = parse_env(&block);
        assert_eq!(env.get("CLAUDECODE").map(String::as_str), Some("1"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("a;b"));
        assert_eq!(env.get("=C:").map(String::as_str), Some("C:\\"));
    }
}
