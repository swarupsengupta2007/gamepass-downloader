//! Windows side of adoption: Gaming Services registry, library roots, services.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use uuid::Uuid;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_SZ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
    RegQueryInfoKeyW,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATUS, SERVICE_STOP, StartServiceW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_TERMINATE, TerminateProcess};

const GAMING_SERVICES: &str = r"SOFTWARE\Microsoft\GamingServices";
const SERVICES: [&str; 2] = ["GamingServices", "GamingServicesNet"];
const PROCESSES: [&str; 2] = ["gamingservices.exe", "gamingservicesnet.exe"];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// `REG_SZ` values of one `HKLM` key; an absent key has none.
fn registry_strings(subkey: &str) -> io::Result<Vec<(String, String)>> {
    let path = wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut key) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(Vec::new());
    }
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let result = (|| {
        let (mut count, mut max_name, mut max_data) = (0u32, 0u32, 0u32);
        let status = unsafe {
            RegQueryInfoKeyW(
                key, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut(),
                std::ptr::null_mut(), std::ptr::null_mut(), &mut count, &mut max_name, &mut max_data,
                std::ptr::null_mut(), std::ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let mut out = Vec::with_capacity(count as usize);
        let mut name = vec![0u16; max_name as usize + 1];
        let mut data = vec![0u8; max_data as usize + 2];
        for index in 0.. {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut kind = 0u32;
            let status = unsafe {
                RegEnumValueW(
                    key, index, name.as_mut_ptr(), &mut name_len, std::ptr::null(), &mut kind,
                    data.as_mut_ptr(), &mut data_len,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            if kind != REG_SZ {
                continue;
            }
            let units: Vec<u16> = data[..data_len as usize]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            out.push((from_wide(&name[..name_len as usize]), from_wide(&units)));
        }
        Ok(out)
    })();
    unsafe { RegCloseKey(key) };
    result
}

/// `{library id}#{CONTENT ID}`, the instance id Gaming Services keys installs by.
pub fn content_id_of_instance(instance: &str) -> Option<Uuid> {
    let (_, id) = instance.split_once('#')?;
    Uuid::parse_str(id.trim_matches(['{', '}'])).ok()
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub instance_id: String,
    pub content_id: Uuid,
    pub checkpoint: serde_json::Value,
}

impl QueueItem {
    pub fn state(&self) -> &str {
        self.checkpoint["State"].as_str().unwrap_or("")
    }

    pub fn operation(&self) -> &str {
        self.checkpoint["Status"]["Operation"].as_str().unwrap_or("")
    }

    pub fn build_id(&self) -> Option<Uuid> {
        self.checkpoint["Status"]["Source"]["Current"]["BuildId"].as_str().and_then(|s| Uuid::parse_str(s).ok())
    }

    /// The service is fetching chunks right now and would race the hand-off.
    pub fn is_streaming(&self) -> bool {
        self.state() == "Running" && self.operation() == "Streaming"
    }
}

/// Install and update items in the Gaming Services queue.
pub fn queue_items() -> io::Result<Vec<QueueItem>> {
    let mut out = Vec::new();
    for (instance_id, json) in registry_strings(&format!(r"{GAMING_SERVICES}\StreamingCheckpoints"))? {
        let (Some(content_id), Ok(checkpoint)) =
            (content_id_of_instance(&instance_id), serde_json::from_str::<serde_json::Value>(&json))
        else {
            continue;
        };
        out.push(QueueItem { instance_id, content_id, checkpoint });
    }
    Ok(out)
}

/// `(package full name, instance id)` for every install Gaming Services finished.
pub fn installed_packages() -> io::Result<Vec<(String, String)>> {
    registry_strings(&format!(r"{GAMING_SERVICES}\PackageRepository\Package"))
}

/// Library folders named by `<drive>:\.GamingRoot` (`RGBX`, count, UTF-16 names).
pub fn gaming_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let Ok(bytes) = std::fs::read(format!("{drive}.GamingRoot")) else {
            continue;
        };
        if bytes.get(..4) != Some(b"RGBX") || bytes.len() < 8 {
            continue;
        }
        let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let units: Vec<u16> = bytes[8..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        for name in units.split(|&u| u == 0).filter(|n| !n.is_empty()).take(count) {
            roots.push(Path::new(&drive).join(String::from_utf16_lossy(name)));
        }
    }
    roots
}

#[derive(Debug, Clone)]
pub struct Install {
    pub dir: PathBuf,
    pub xvi: PathBuf,
    pub plans: Vec<PathBuf>,
    pub smd: Option<PathBuf>,
    pub header_cache: PathBuf,
}

impl Install {
    pub fn content_dir(&self) -> PathBuf {
        self.dir.join("Content")
    }

    /// The streaming plan for one build: `<content id>.<build id>.xsp`.
    pub fn plan_for(&self, build: Uuid) -> Option<&PathBuf> {
        let suffix = format!(".{}.XSP", build.hyphenated().to_string().to_uppercase());
        self.plans.iter().find(|p| p.to_string_lossy().to_uppercase().ends_with(&suffix))
    }
}

pub fn file_id(content_id: Uuid) -> String {
    content_id.hyphenated().to_string().to_uppercase()
}

/// The folder Gaming Services created for this content id, under any library root.
pub fn find_install(content_id: Uuid) -> Option<Install> {
    let id = file_id(content_id);
    for root in gaming_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let xvi = dir.join(format!("{id}.xvi"));
            if !xvi.is_file() {
                continue;
            }
            let plans = std::fs::read_dir(&dir)
                .map(|e| {
                    e.flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            let name = p.file_name().unwrap_or_default().to_string_lossy().to_uppercase();
                            name.starts_with(&format!("{id}.")) && name.ends_with(".XSP")
                        })
                        .collect()
                })
                .unwrap_or_default();
            let smd = Some(dir.join(format!("{id}.smd"))).filter(|p| p.is_file());
            return Some(Install { header_cache: dir.join(&id), dir, xvi, plans, smd });
        }
    }
    None
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

fn gaming_process_ids() -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let name = from_wide(&entry.szExeFile).to_ascii_lowercase();
            if PROCESSES.contains(&name.as_str()) {
                out.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    out
}

fn with_service(name: &str, access: u32, f: impl FnOnce(*mut c_void) -> bool) -> bool {
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return false;
        }
        let service = OpenServiceW(manager, wide(name).as_ptr(), access);
        let ok = !service.is_null() && f(service);
        if !service.is_null() {
            CloseServiceHandle(service);
        }
        CloseServiceHandle(manager);
        ok
    }
}

pub fn start_gaming_services() {
    for name in SERVICES {
        with_service(name, SERVICE_START, |s| unsafe { StartServiceW(s, 0, std::ptr::null()) != 0 });
    }
}

/// Gaming Services is stopped while this lives and started again on drop.
/// A queued install can start the service again on its own, so callers open
/// the files they rewrite exclusively and retry if that fails.
pub struct ServicesStopped(());

impl Drop for ServicesStopped {
    fn drop(&mut self) {
        start_gaming_services();
    }
}

pub fn stop_gaming_services(timeout: Duration) -> io::Result<ServicesStopped> {
    let started = Instant::now();
    loop {
        for name in SERVICES {
            with_service(name, SERVICE_STOP | SERVICE_QUERY_STATUS, |s| unsafe {
                let mut status: SERVICE_STATUS = std::mem::zeroed();
                ControlService(s, SERVICE_CONTROL_STOP, &mut status) != 0
            });
        }
        let pids = gaming_process_ids();
        if pids.is_empty() {
            return Ok(ServicesStopped(()));
        }
        // A service can report Stopped while its process is still finishing.
        if started.elapsed() > Duration::from_secs(10) {
            for pid in pids {
                unsafe {
                    let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if !process.is_null() {
                        TerminateProcess(process, 1);
                        CloseHandle(process);
                    }
                }
            }
        }
        if started.elapsed() > timeout {
            start_gaming_services();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Gaming Services did not stop"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Open for read and write with no sharing, so the service cannot touch the
/// file while it is being rewritten.
pub fn open_exclusive(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(create).share_mode(0).open(path)
}

/// Gaming Services denies `BUILTIN\Users` on a file while it writes it, and a
/// paused write keeps that deny. Administrators are members of Users, so even
/// an elevated process is refused, down to reading the ACL. Taking ownership
/// is always allowed elevated; the owner may then drop the deny.
pub fn clear_write_lock(path: &Path) -> io::Result<()> {
    let path = path.to_string_lossy();
    let status = Command::new("takeown").args(["/F", &path, "/A"]).stdout(std::process::Stdio::null()).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("could not take ownership of {path}")));
    }
    icacls(&[&path, "/remove:d", "*S-1-5-32-545"])
}

/// Run `icacls`; it reports per-file failures with `/C`, which callers accept.
pub fn icacls(args: &[&str]) -> io::Result<()> {
    let status = Command::new("icacls").args(args).stdout(std::process::Stdio::null()).status()?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("icacls exited with {status}"))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_carry_the_content_id() {
        assert_eq!(
            content_id_of_instance("{30FF2225-7FE8-4CD9-AFFB-FB48EA541B1F}#{7B0ECD90-9694-4CE7-8E49-7454781D3CCE}"),
            Some(Uuid::parse_str("7b0ecd90-9694-4ce7-8e49-7454781d3cce").unwrap())
        );
        assert_eq!(content_id_of_instance("Microsoft.Patagonia_8wekyb3d8bbwe"), None);
    }
}
