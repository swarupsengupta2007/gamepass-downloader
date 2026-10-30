//! Everything the window does runs a copy of this exe with an `xodus-cli`
//! command: `queue`, `account`, `resolve-package`, `login`, `streaming` and
//! `adopt`. A download in its own process can be stopped, and one going
//! wrong cannot take the window with it.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

pub fn cli_path() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(cli_path());
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// The last stderr line that is an error, not a diagnostic or log record.
fn error_text(stderr: &[u8], fallback: &str) -> String {
    let noise = |l: &str| {
        l.is_empty()
            || l.starts_with("NEXUS_")
            || l.starts_with("CDN root ")
            // tracing lines: `2026-10-26T19:09:19Z  WARN target: message`
            || l.get(..4).is_some_and(|y| y.bytes().all(|b| b.is_ascii_digit())) && l.contains('T') && l.contains("Z ")
    };
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !noise(l))
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

fn run_json(args: &[&str]) -> Result<Value, String> {
    let output = command(args).output().map_err(|e| format!("cannot start {}: {e}", cli_path().display()))?;
    if !output.status.success() {
        return Err(error_text(&output.stderr, &format!("{} failed", args[0])));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| format!("{} returned unreadable output", args[0]))
}

#[derive(Debug, Clone)]
pub struct QueueRow {
    pub content_id: String,
    pub store_id: String,
    pub state: String,
    pub operation: String,
    pub total_bytes: u64,
    pub streamed_bytes: u64,
    pub install_dir: Option<String>,
    pub ready: bool,
    pub not_ready_reason: Option<String>,
}

impl QueueRow {
    pub fn xbox_is_downloading(&self) -> bool {
        self.state == "Running" && self.operation == "Streaming"
    }
}

#[derive(Debug, Clone, Default)]
pub struct Queue {
    pub rows: Vec<QueueRow>,
    /// Content ids (lowercase) Gaming Services finished installing.
    pub installed: HashSet<String>,
}

pub fn read_queue() -> Result<Queue, String> {
    let value = run_json(&["queue", "--json"])?;
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let rows = value["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| QueueRow {
            content_id: text(&item["contentId"]).to_lowercase(),
            store_id: text(&item["storeId"]),
            state: text(&item["state"]),
            operation: text(&item["operation"]),
            total_bytes: item["totalBytes"].as_u64().unwrap_or(0),
            streamed_bytes: item["streamedBytes"].as_u64().unwrap_or(0),
            install_dir: item["installDir"].as_str().map(str::to_owned),
            ready: item["ready"].as_bool().unwrap_or(false),
            not_ready_reason: item["notReadyReason"].as_str().map(str::to_owned),
        })
        .collect();
    let installed = value["installed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["contentId"].as_str().map(str::to_lowercase))
        .collect();
    Ok(Queue { rows, installed })
}

/// The signed-in Microsoft account, if any.
pub fn account() -> Result<Option<String>, String> {
    let value = run_json(&["account", "--json"])?;
    Ok(value["username"].as_str().filter(|_| value["stored"] == true).map(str::to_owned))
}

/// The Xbox side of the signed-in account.
#[derive(Clone)]
pub struct Profile {
    pub gamertag: String,
    /// Gamerpic, square RGBA, already scaled down for the window.
    pub picture: Option<eframe::egui::ColorImage>,
    /// The best Game Pass plan the account has, when the membership is active.
    pub plan: Option<String>,
    /// The account's country, the Store market it buys in.
    pub country: Option<String>,
}

/// Higher plans include the lower ones, and an account can hold several.
const PLAN_ORDER: [&str; 5] = ["CFQ7TTC0KHS0", "CFQ7TTC0P85B", "CFQ7TTC0KGQ8", "CFQ7TTC0K5DJ", "CFQ7TTC0K6L8"];

/// `profile --json` signs in to Xbox with the stored account; a few seconds.
pub fn profile() -> Result<Profile, String> {
    let value = run_json(&["profile", "--json"])?;
    let gamertag = value["gamertag"].as_str().unwrap_or_default().to_owned();
    let picture = value["gamerpicDataUrl"].as_str().and_then(picture);
    let subscription = &value["subscription"];
    let plan = (subscription["status"] == "active").then(|| {
        let held: Vec<&str> = subscription["productIds"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let id = PLAN_ORDER.into_iter().find(|id| held.contains(id))?;
        let name = xodus::models::xbox::subscriptions::GAMEPASS_SUBS.iter().find(|s| s.id == id)?.name;
        Some(name.trim_start_matches("Xbox ").to_owned())
    }).flatten();
    let country = value["country"].as_str().map(str::to_owned);
    Ok(Profile { gamertag, picture, plan, country })
}

fn picture(data_url: &str) -> Option<eframe::egui::ColorImage> {
    use base64::Engine;
    const SIDE: u32 = 160;
    let (_, encoded) = data_url.split_once(";base64,")?;
    let bytes = base64::prelude::BASE64_STANDARD.decode(encoded).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let image = image.resize_to_fill(SIDE, SIDE, image::imageops::FilterType::Lanczos3).to_rgba8();
    Some(eframe::egui::ColorImage::from_rgba_unmultiplied([SIDE as usize; 2], image.as_raw()))
}

/// Removes the stored Microsoft account. The device identity stays.
pub fn logout() -> Result<(), String> {
    let output = command(&["logout"]).output().map_err(|e| format!("cannot start {}: {e}", cli_path().display()))?;
    if output.status.success() { Ok(()) } else { Err(error_text(&output.stderr, "Sign-out failed.")) }
}

pub fn title(store_id: &str, content_id: &str, market: &str) -> Option<String> {
    let value = run_json(&["resolve-package", store_id, "--json", "--market", market]).ok()?;
    let choices = value["choices"].as_array()?;
    choices
        .iter()
        .find(|c| c["contentId"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(content_id)))
        .or(choices.first())
        .and_then(|c| c["title"].as_str())
        .map(str::to_owned)
}

/// Downloads go on the install's drive, so handing them off is a rename.
pub fn staging_dir(install_dir: &str, content_id: &str) -> PathBuf {
    let root = Path::new(install_dir).components().next().map(|c| PathBuf::from(c.as_os_str())).unwrap_or_default();
    root.join("\\XboxDownloads").join(content_id.to_uppercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staging {
    Empty,
    Partial,
    /// Another `streaming` process holds the folder lock.
    Busy,
    Finished,
}

pub fn staging_state(dir: &Path) -> Staging {
    if dir.join(".xodus-streaming-complete.json").is_file() {
        return Staging::Finished;
    }
    let lock = dir.join(".nexus-streaming.lock");
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&lock) {
        if fs2::FileExt::try_lock_exclusive(&file).is_err() {
            return Staging::Busy;
        }
        fs2::FileExt::unlock(&file).ok();
    }
    // The prefix keeps its `-tmp` name until a run finishes.
    let started = [".nexus-streaming-v1.jsonl", ".xodus-streaming-tmp.msixvc", ".xodus-streaming.msixvc"];
    if started.iter().any(|name| dir.join(name).is_file()) { Staging::Partial } else { Staging::Empty }
}

#[derive(Debug, Clone, Default)]
pub struct DownloadState {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
    pub bytes_per_second: u64,
    pub paused: bool,
    pub can_pause: bool,
    pub finished: Option<Result<(), String>>,
}

/// Progress a `streaming` process in another window mirrors into its folder.
/// `None` when there is no file or it has not been updated for a while.
pub fn read_status(dir: &Path) -> Option<DownloadState> {
    let record: Value = serde_json::from_slice(&std::fs::read(dir.join(".xodus-streaming-progress.json")).ok()?).ok()?;
    let updated = record["updatedAt"].as_i64()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
    if now - updated > 15_000 {
        return None;
    }
    Some(DownloadState {
        phase: record["phase"].as_str().unwrap_or_default().to_owned(),
        completed: record["completedBytes"].as_u64().unwrap_or(0),
        total: record["totalBytes"].as_u64(),
        bytes_per_second: record["bytesPerSecond"].as_u64().unwrap_or(0),
        paused: record["paused"].as_bool().unwrap_or(false),
        can_pause: false,
        finished: None,
    })
}

pub struct Download {
    pub state: Arc<Mutex<DownloadState>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl Download {
    pub fn start(store_id: &str, content_id: &str, dir: &Path, market: &str, parallel: usize, repaint: Repaint) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let mut child = command(&[
            "streaming", store_id, &dir.to_string_lossy(), "--selected-content-id", content_id,
            "--market", market, "--parallel", &parallel.to_string(), "--progress-json", "--control-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", cli_path().display()))?;
        let state = Arc::new(Mutex::new(DownloadState { phase: "starting".into(), ..Default::default() }));
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let errors = std::thread::spawn(move || {
            let mut text = Vec::new();
            stderr.read_to_end(&mut text).ok();
            text
        });
        let progress = state.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Some(json) = line.strip_prefix("NEXUS_PROGRESS ") else { continue };
                let Ok(record) = serde_json::from_str::<Value>(json) else { continue };
                let mut s = progress.lock().unwrap();
                s.phase = record["phase"].as_str().unwrap_or_default().to_owned();
                s.completed = record["completedBytes"].as_u64().unwrap_or(s.completed);
                s.total = record["totalBytes"].as_u64();
                s.bytes_per_second = record["bytesPerSecond"].as_u64().unwrap_or(0);
                s.paused = record["paused"].as_bool().unwrap_or(false);
                s.can_pause = record["canPause"].as_bool().unwrap_or(false);
                drop(s);
                repaint();
            }
            let status = child.wait();
            let stderr = errors.join().unwrap_or_default();
            let mut s = progress.lock().unwrap();
            s.finished = Some(match status {
                Ok(status) if status.success() && s.phase == "complete" => Ok(()),
                Ok(_) => Err(error_text(&stderr, "The download stopped.")),
                Err(e) => Err(e.to_string()),
            });
            drop(s);
            repaint();
        });
        Ok(Self { state, stdin })
    }

    fn control(&self, command: &str) {
        if let Some(stdin) = self.stdin.lock().unwrap().as_mut() {
            writeln!(stdin, "{}", serde_json::json!({"schema": 1, "command": command})).ok();
            stdin.flush().ok();
        }
    }

    pub fn pause(&self) {
        self.control("pause");
    }

    pub fn resume(&self) {
        self.control("resume");
    }
}

#[derive(Debug, Clone, Default)]
pub struct AdoptState {
    pub message: String,
    pub done: usize,
    pub total: usize,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub finished: Option<Result<String, String>>,
}

pub struct Adopt {
    pub state: Arc<Mutex<AdoptState>>,
}

impl Adopt {
    /// Run `adopt` elevated (one UAC prompt) and follow its log.
    pub fn start(content_id: &str, dir: &Path, keep_download: bool, repaint: Repaint) -> Self {
        let state = Arc::new(Mutex::new(AdoptState { message: "Waiting for administrator approval".into(), ..Default::default() }));
        let log = std::env::temp_dir().join("gamepass-downloader").join(format!("adopt-{content_id}.jsonl"));
        let copy = if keep_download { " --copy" } else { "" };
        let params = format!("adopt {content_id} \"{}\" --json --log \"{}\"{copy}", dir.display(), log.display());
        let progress = state.clone();
        std::thread::spawn(move || {
            std::fs::create_dir_all(log.parent().unwrap()).ok();
            std::fs::remove_file(&log).ok();
            let result = run_elevated(&cli_path(), &params, |done| follow_log(&log, &progress, done, &repaint));
            let mut s = progress.lock().unwrap();
            if s.finished.is_none() {
                s.finished = Some(Err(match result {
                    Ok(code) => format!("adopt stopped with exit code {code}"),
                    Err(e) => e,
                }));
            }
            drop(s);
            repaint();
        });
        Self { state }
    }
}

fn follow_log(log: &Path, state: &Mutex<AdoptState>, process_done: &dyn Fn() -> bool, repaint: &Repaint) {
    let mut offset = 0u64;
    let mut partial = String::new();
    loop {
        let exited = process_done();
        if let Ok(mut file) = std::fs::File::open(log) {
            file.seek(SeekFrom::Start(offset)).ok();
            let mut chunk = String::new();
            if let Ok(read) = file.read_to_string(&mut chunk) {
                offset += read as u64;
                partial.push_str(&chunk);
            }
        }
        while let Some(end) = partial.find('\n') {
            let line: String = partial.drain(..=end).collect();
            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else { continue };
            let message = event["message"].as_str().unwrap_or_default().to_owned();
            let mut s = state.lock().unwrap();
            match event["event"].as_str() {
                Some("step") => s.message = message,
                Some("progress") => {
                    s.done = event["done"].as_u64().unwrap_or(0) as usize;
                    s.total = event["total"].as_u64().unwrap_or(0) as usize;
                    s.done_bytes = event["doneBytes"].as_u64().unwrap_or(0);
                    s.total_bytes = event["totalBytes"].as_u64().unwrap_or(0);
                }
                Some("done") => s.finished = Some(Ok(message)),
                Some("error") => s.finished = Some(Err(message)),
                _ => {}
            }
            drop(s);
            repaint();
        }
        if exited {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(windows)]
fn run_elevated(exe: &Path, params: &str, mut wait: impl FnMut(&dyn Fn() -> bool)) -> Result<u32, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file, params) = (wide("runas"), wide(&exe.to_string_lossy()), wide(params));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = unsafe { GetLastError() };
        return Err(if error == 1223 {
            "Administrator approval was declined.".into()
        } else {
            format!("could not start adopt as administrator (error {error})")
        });
    }
    let process = info.hProcess;
    wait(&|| unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0);
    let mut code = 0u32;
    unsafe {
        GetExitCodeProcess(process, &mut code);
        CloseHandle(process);
    }
    Ok(code)
}

#[cfg(not(windows))]
fn run_elevated(_: &Path, _: &str, _: impl FnMut(&dyn Fn() -> bool)) -> Result<u32, String> {
    Err("handing a game to the Xbox app needs Windows".into())
}

pub struct Login {
    pub result: Arc<Mutex<Option<Result<(), String>>>>,
    pid: Arc<Mutex<Option<u32>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Login {
    /// `login` opens the Microsoft sign-in page in Edge or Chrome. Its
    /// messages go to a file: the browser can inherit a pipe and hold it open
    /// after `login` exits.
    pub fn start(repaint: Repaint) -> Self {
        let result = Arc::new(Mutex::new(None));
        let pid = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (out, child_pid, was_cancelled) = (result.clone(), pid.clone(), cancelled.clone());
        std::thread::spawn(move || {
            let log = std::env::temp_dir().join("gamepass-downloader").join("login.log");
            std::fs::create_dir_all(log.parent().unwrap()).ok();
            let status = std::fs::File::create(&log)
                .and_then(|file| command(&["login"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(file).spawn())
                .and_then(|mut child| {
                    *child_pid.lock().unwrap() = Some(child.id());
                    let status = child.wait();
                    *child_pid.lock().unwrap() = None;
                    status
                });
            let stderr = std::fs::read(&log).unwrap_or_default();
            *out.lock().unwrap() = Some(match status {
                Ok(s) if s.success() => Ok(()),
                _ if was_cancelled.load(std::sync::atomic::Ordering::SeqCst) => Ok(()),
                Ok(_) => Err(error_text(&stderr, "Sign-in did not finish.")),
                Err(e) => Err(e.to_string()),
            });
            repaint();
        });
        Self { result, pid, cancelled }
    }

    pub fn running(&self) -> bool {
        self.result.lock().unwrap().is_none()
    }

    /// Ends `login` and the browser window it opened.
    pub fn cancel(&self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap() {
            let mut taskkill = Command::new("taskkill");
            taskkill.args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                taskkill.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            taskkill.status().ok();
        }
    }
}

pub fn open_xbox_app() {
    Command::new("explorer.exe")
        .arg(r"shell:AppsFolder\Microsoft.GamingApp_8wekyb3d8bbwe!Microsoft.Xbox.App")
        .spawn()
        .ok();
}

/// What is left in a download folder after a move: cache, journal and marker.
pub fn remove_download(dir: &Path) -> Result<(), String> {
    std::fs::remove_dir_all(dir).map_err(|e| format!("could not remove {}: {e}", dir.display()))
}

/// Two-letter Store market from the Windows region.
pub fn default_market() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::GetUserDefaultGeoName;
        let mut buf = [0u16; 16];
        let len = unsafe { GetUserDefaultGeoName(buf.as_mut_ptr(), buf.len() as i32) };
        if len > 1 {
            let name = String::from_utf16_lossy(&buf[..len as usize - 1]);
            if name.len() == 2 {
                return name.to_uppercase();
            }
        }
    }
    "US".into()
}

pub fn titles_cache() -> Arc<Mutex<HashMap<String, Option<String>>>> {
    Arc::new(Mutex::new(HashMap::new()))
}
