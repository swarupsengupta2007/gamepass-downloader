//! Move a finished `streaming` download into the Xbox app's install folder and
//! mark every streaming chunk resident, so Resume in the Xbox app finalizes the
//! install instead of downloading it.
//!
//! Needs an elevated process: the residency index and header cache belong to
//! Gaming Services, which has to be stopped while they are rewritten.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use crate::xbox::sidecars::{inspect_residency, mark_resident, parse_plan, parse_segments};
use crate::xbox::system;
use crate::xbox::{self as xbox, PREFIX_FILE};

/// Files already moved into `Content` by an earlier, interrupted run.
const MOVED_LOG: &str = ".xodus-adopt-moved.tsv";
const USERS_SID: &str = "*S-1-5-32-545";

struct Report {
    json: bool,
    log: Option<File>,
}

impl Report {
    fn emit(&mut self, value: serde_json::Value) {
        if self.json {
            println!("{value}");
        }
        if let Some(log) = self.log.as_mut() {
            writeln!(log, "{value}").ok();
            log.flush().ok();
        }
    }

    fn step(&mut self, step: &str, message: impl Into<String>) {
        let message = message.into();
        if !self.json {
            println!("{message}");
        }
        self.emit(json!({"schema": 1, "event": "step", "step": step, "message": message}));
    }

    fn progress(&mut self, step: &str, (done, total): (usize, usize), (done_bytes, total_bytes): (u64, u64)) {
        self.emit(json!({"schema": 1, "event": "progress", "step": step, "done": done, "total": total,
            "doneBytes": done_bytes, "totalBytes": total_bytes}));
    }

    fn fail(&mut self, code: &str, message: impl Into<String>) -> ExitCode {
        let message = message.into();
        eprintln!("{message}");
        self.emit(json!({"schema": 1, "event": "error", "code": code, "message": message}));
        ExitCode::FAILURE
    }
}

struct Failure(&'static str, String);

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Failure("io", error.to_string())
    }
}

pub fn run(content_id: String, source: String, copy: bool, json_output: bool, log: Option<String>) -> ExitCode {
    let log = log.and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());
    let mut report = Report { json: json_output, log };
    match adopt(&content_id, Path::new(&source), copy, &mut report) {
        Ok(()) => {
            report.emit(json!({"schema": 1, "event": "done",
                "message": "Open the Xbox app, go to the install queue, and choose Resume installation for this game. \
                    It downloads only the game's executables, which the Xbox app has to write itself, then finishes."}));
            if !json_output {
                println!("Done. In the Xbox app install queue, choose Resume installation for this game.");
            }
            ExitCode::SUCCESS
        }
        Err(Failure(code, message)) => report.fail(code, message),
    }
}

fn adopt(content_id: &str, source: &Path, copy: bool, report: &mut Report) -> Result<(), Failure> {
    if !system::is_elevated() {
        return Err(Failure("not-elevated", "adopt has to run as administrator".into()));
    }
    let content_id = Uuid::parse_str(content_id).map_err(|_| Failure("usage", format!("{content_id} is not a content id")))?;

    report.step("check", "Checking the download and the Xbox app install");
    let download = xbox::finished_download(source).map_err(|e| Failure("download-incomplete", e.to_string()))?;
    if download.content_id != content_id {
        return Err(Failure("build-mismatch", format!("the download is {}, not {content_id}", download.content_id)));
    }
    let item = system::queue_items()?
        .into_iter()
        .find(|item| item.content_id == content_id)
        .ok_or_else(|| Failure("not-queued", "this game is not in the Xbox app install queue".into()))?;
    if item.is_streaming() {
        return Err(Failure("streaming", "the Xbox app is downloading this game; pause it in the install queue first".into()));
    }
    let install = system::find_install(content_id)
        .ok_or_else(|| Failure("not-queued", "Gaming Services has not created the install folder yet".into()))?;
    if item.build_id().is_some_and(|queued| queued != download.build_id) {
        return Err(Failure("build-mismatch", format!(
            "the Xbox app queued build {}, the download is build {} ({}). Download the game again.",
            item.build_id().unwrap(), download.build_id, download.version
        )));
    }
    let plan_path = install.plan_for(download.build_id).ok_or_else(|| {
        Failure("build-mismatch", format!("no streaming plan for build {} beside the install", download.build_id))
    })?;
    let plan = parse_plan(&fs::read(plan_path)?).map_err(|e| Failure("layout", e))?;
    if plan.content_id != content_id || plan.build_id != download.build_id {
        return Err(Failure("build-mismatch", "the streaming plan is for a different package".into()));
    }
    let content_dir = install.content_dir();
    let recorded = format!(r"\??\{}", content_dir.display());
    let residency = inspect_residency(&fs::read(&install.xvi)?, &plan).map_err(|e| Failure("layout", e))?;
    if let Some(path) = &residency.content_path
        && !path.eq_ignore_ascii_case(&recorded)
    {
        return Err(Failure("layout", format!("Gaming Services recorded {path}, the install folder is {recorded}")));
    }
    let cache_len = fs::metadata(&install.header_cache).map(|m| m.len()).unwrap_or(0);
    if cache_len != 0 && cache_len != download.resident_len {
        return Err(Failure("layout", format!(
            "the header cache is {cache_len} bytes, the package header is {} bytes", download.resident_len
        )));
    }
    let prefix = source.join(PREFIX_FILE);
    if fs::metadata(&prefix)?.len() < download.resident_len {
        return Err(Failure("download-incomplete", "the downloaded package header is truncated".into()));
    }
    // Executables the package keeps encrypted on disk: only Gaming Services can
    // write them in a form Windows will launch, so their chunks stay missing.
    let service_files = xbox::service_written_files(&prefix).map_err(|e| Failure("layout", e.to_string()))?;
    if let Some(smd) = &install.smd {
        check_segments(&fs::read(smd)?, source, &content_dir, &service_files)?;
    }
    report.step("check", format!(
        "{} of {} streaming chunks are still missing in {}",
        residency.missing, residency.entries, install.dir.display()
    ));
    let pages: Vec<_> = service_files.iter().map(|f| f.pages.clone()).collect();
    let left_missing = plan.records_covering(&pages);
    if !service_files.is_empty() && left_missing.is_empty() {
        return Err(Failure("layout", "the streaming plan does not cover the package's encrypted files".into()));
    }
    let left_bytes: u64 = left_missing.iter().map(|&i| (plan.pages[i].end - plan.pages[i].start) * 4096).sum();
    if !service_files.is_empty() {
        let names: Vec<_> = service_files.iter().map(|f| f.path.as_str()).collect();
        report.step("check", format!(
            "The Xbox app writes {} itself; Resume downloads those {} chunks ({:.1} MB)",
            names.join(", "), left_missing.len(), left_bytes as f64 / 1e6
        ));
    }

    let mut pending = move_files(source, &content_dir, copy, &service_files, report)?;

    // Hold the residency index and header cache exclusively while they are
    // rewritten. The queued install can restart the service at any moment.
    let started = Instant::now();
    loop {
        report.step("stop-services", "Stopping Gaming Services");
        let stopped = system::stop_gaming_services(Duration::from_secs(60))?;
        let open = |path: &Path, create: bool| match system::open_exclusive(path, create) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                system::clear_write_lock(path)?;
                system::open_exclusive(path, create)
            }
            other => other,
        };
        let handles = open(&install.xvi, false).and_then(|xvi| open(&install.header_cache, true).map(|cache| (xvi, cache)));
        let (mut xvi, mut cache) = match handles {
            Ok(handles) => handles,
            // ERROR_SHARING_VIOLATION: the service started again and holds it.
            Err(error) if error.raw_os_error() == Some(32) && started.elapsed() < Duration::from_secs(120) => {
                drop(stopped);
                report.step("stop-services", "Gaming Services started again; retrying");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if !pending.is_empty() {
            report.step("move", format!("Retrying {} files with Gaming Services stopped", pending.len()));
            pending = place_all(source, &content_dir, copy, pending, report)?;
            if let Some(Pending { path, error, .. }) = pending.first() {
                return Err(Failure("io", format!("could not place {path}: {error} ({} files left)", pending.len())));
            }
        }
        // A copy from an earlier run, or one the service had started, lacks the
        // attribute or is partial; the service writes it again from scratch.
        for file in &service_files {
            remove_file(&content_dir.join(&file.path))?;
        }
        report.step("header-cache", "Writing the package header cache");
        write_header_cache(&prefix, &mut cache, download.resident_len)?;
        report.step("residency", "Marking the downloaded streaming chunks resident");
        let mut bytes = Vec::new();
        xvi.read_to_end(&mut bytes)?;
        mark_resident(&mut bytes, &plan, &recorded, &left_missing).map_err(|e| Failure("layout", e))?;
        xvi.seek(SeekFrom::Start(0))?;
        xvi.write_all(&bytes)?;
        xvi.sync_all()?;
        drop((xvi, cache));
        report.step("start-services", "Starting Gaming Services");
        drop(stopped);
        break;
    }

    let after = inspect_residency(&fs::read(&install.xvi)?, &plan).map_err(|e| Failure("layout", e))?;
    if after.missing != left_missing.len() {
        return Err(Failure("layout", format!(
            "{} chunks are marked missing after the update, expected {}", after.missing, left_missing.len()
        )));
    }
    Ok(())
}

fn remove_file(path: &Path) -> io::Result<()> {
    let remove = || match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    };
    match remove() {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            system::clear_write_lock(path)?;
            if let Ok(meta) = fs::metadata(path) {
                let mut permissions = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                fs::set_permissions(path, permissions)?;
            }
            remove()
        }
        other => other,
    }
}

/// Every file Gaming Services expects must be in the download, or already in
/// `Content` from an earlier run, at its exact size. Files the service writes
/// itself are left out.
fn check_segments(smd: &[u8], source: &Path, content: &Path, service_files: &[xbox::ServiceFile]) -> Result<(), Failure> {
    let segments = parse_segments(smd).map_err(|e| Failure("layout", e))?;
    let moved = moved_before(source);
    let missing: Vec<_> = segments
        .iter()
        .filter(|s| !service_files.iter().any(|f| f.path.eq_ignore_ascii_case(&s.path)))
        .filter(|s| {
            let in_download = fs::metadata(source.join(&s.path)).is_ok_and(|m| m.len() == s.size);
            let in_content = moved.iter().any(|(p, size)| *p == s.path && *size == s.size)
                && fs::metadata(content.join(&s.path)).is_ok_and(|m| m.len() == s.size);
            !in_download && !in_content
        })
        .map(|s| s.path.as_str())
        .collect();
    if let Some(first) = missing.first() {
        return Err(Failure("download-incomplete", format!(
            "{} files the Xbox app expects are missing or a different size, for example {first}", missing.len()
        )));
    }
    Ok(())
}

fn write_header_cache(prefix: &Path, cache: &mut File, len: u64) -> io::Result<()> {
    let mut source = File::open(prefix)?.take(len);
    cache.seek(SeekFrom::Start(0))?;
    let written = io::copy(&mut source, cache)?;
    if written != len {
        return Err(io::Error::other("the package header ended early"));
    }
    cache.set_len(len)?;
    cache.sync_all()
}

/// Earlier runs' moves, so an interrupted adoption can continue.
fn moved_before(source: &Path) -> Vec<(String, u64)> {
    let Ok(file) = File::open(source.join(MOVED_LOG)) else {
        return Vec::new();
    };
    io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let (size, path) = line.split_once('\t')?;
            Some((path.to_owned(), size.parse().ok()?))
        })
        .collect()
}

/// A file still to place, and why the last attempt failed.
struct Pending {
    path: String,
    size: u64,
    error: String,
}

fn move_files(
    source: &Path,
    content: &Path,
    copy: bool,
    service_files: &[xbox::ServiceFile],
    report: &mut Report,
) -> Result<Vec<Pending>, Failure> {
    let mut files = xbox::downloaded_files(source)?;
    files.retain(|(path, _)| !service_files.iter().any(|f| f.path.eq_ignore_ascii_case(path)));
    let mut already = std::collections::HashSet::new();
    for (path, size) in moved_before(source) {
        let placed = fs::metadata(content.join(&path)).is_ok_and(|m| m.len() == size);
        if placed {
            already.insert(path);
        } else if !files.iter().any(|(p, _)| *p == path) {
            return Err(Failure("download-incomplete", format!("{path} is neither in the download nor in Content")));
        }
    }
    // An earlier run placed these (a copy leaves the download in place).
    files.retain(|(path, _)| !already.contains(path));
    if !already.is_empty() {
        report.step("move", format!("{} files are already in place from an earlier run", already.len()));
    }
    if files.is_empty() {
        report.step("move", "Every file is already in place");
        return Ok(Vec::new());
    }

    if content.is_dir() {
        // Gaming Services denies Users on a file while it writes it. A paused
        // or stopped write can leave that deny behind.
        report.step("permissions", "Clearing leftover write locks in Content");
        system::icacls(&[&content.to_string_lossy(), "/remove:d", USERS_SID, "/T", "/C", "/Q"]).ok();
    } else {
        fs::create_dir_all(content)?;
        // What Gaming Services grants on a Content folder it creates itself.
        system::icacls(&[
            &content.to_string_lossy(),
            "/grant", "*S-1-15-2-1:(OI)(CI)(RX)", "*S-1-19-512-4096:(OI)(CI)(RX)", "/C", "/Q",
        ])
        .ok();
    }

    let verb = if copy { "Copying" } else { "Moving" };
    report.step("move", format!("{verb} {} files into {}", files.len(), content.display()));
    let files = files.into_iter().map(|(path, size)| Pending { path, size, error: String::new() }).collect();
    let pending = place_all(source, content, copy, files, report)?;
    if !copy {
        // Renamed files keep the download folder's permissions; take Content's.
        report.step("permissions", "Applying the install folder's permissions");
        system::icacls(&[&format!("{}\\*", content.display()), "/reset", "/T", "/C", "/Q"]).ok();
    }
    Ok(pending)
}

/// Files and bytes placed so far, reported at most twice a second. Bytes move
/// inside a large copy too, so a few big archives up front do not look stuck.
struct Progress<'a> {
    report: &'a mut Report,
    files: (usize, usize),
    bytes: (u64, u64),
    last: Instant,
}

impl Progress<'_> {
    fn emit(&mut self, force: bool) {
        if force || self.last.elapsed() > Duration::from_millis(500) {
            self.report.progress("move", self.files, self.bytes);
            self.last = Instant::now();
        }
    }
}

/// Place each file, returning the ones that failed with their error.
fn place_all(source: &Path, content: &Path, copy: bool, files: Vec<Pending>, report: &mut Report) -> Result<Vec<Pending>, Failure> {
    let mut log = OpenOptions::new().create(true).append(true).open(source.join(MOVED_LOG))?;
    let total_bytes = files.iter().map(|f| f.size).sum();
    let mut progress = Progress { report, files: (0, files.len()), bytes: (0, total_bytes), last: Instant::now() };
    let mut failed = Vec::new();
    for file in files {
        let (from, to) = (source.join(&file.path), content.join(&file.path));
        let before = progress.bytes.0;
        let placed = place(&from, &to, copy, &mut |copied| {
            progress.bytes.0 = before + copied;
            progress.emit(false);
        });
        match placed {
            Ok(size) => {
                writeln!(log, "{size}\t{}", file.path)?;
                progress.bytes.0 = before + file.size;
            }
            Err(error) => {
                // Retried later; its bytes are not placed yet.
                progress.bytes.0 = before;
                failed.push(Pending { error: error.to_string(), ..file });
            }
        }
        progress.files.0 += 1;
        progress.emit(progress.files.0 == progress.files.1);
    }
    log.sync_all()?;
    Ok(failed)
}

fn place(from: &Path, to: &Path, copy: bool, copied: &mut dyn FnMut(u64)) -> io::Result<u64> {
    let size = fs::metadata(from)?.len();
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::metadata(to)
        && meta.permissions().readonly()
    {
        let mut permissions = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(to, permissions)?;
    }
    if copy {
        copy_file(from, to, copied)?;
        return Ok(size);
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(size),
        // Different volume: copy, then drop the source.
        Err(error) if error.raw_os_error() == Some(17) => {
            copy_file(from, to, copied)?;
            fs::remove_file(from)?;
            Ok(size)
        }
        Err(error) => Err(error),
    }
}

/// `fs::copy` with progress: 8 MiB at a time, reporting bytes written so far.
fn copy_file(from: &Path, to: &Path, copied: &mut dyn FnMut(u64)) -> io::Result<()> {
    let mut input = File::open(from)?;
    let mut output = File::create(to)?;
    output.set_len(input.metadata()?.len())?;
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let mut done = 0u64;
    loop {
        let read = input.read(&mut buf)?;
        if read == 0 {
            break;
        }
        output.write_all(&buf[..read])?;
        done += read as u64;
        copied(done);
    }
    output.set_len(done)?;
    Ok(())
}
