//! Check a game folder against the package hash tree, and fetch damaged files
//! again. The hash tree holds SHA-256 of every encrypted page, so each page on
//! disk is encrypted again with the account's content key and compared. A CDN
//! location has served damaged bytes for some ranges, and downloads before
//! page checks wrote them into games without an error.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use msixvc::layout::PAGE_SIZE;
use msixvc::xvd::{PageCheck, SegmentFile, XvdFile};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use xodus::tokens::TokenManager;

use super::streaming::{content_key, package_urls};

const PAGE: u64 = PAGE_SIZE as u64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Ok,
    Missing,
    WrongSize(u64),
    /// Pages that fail their hash, by index in the file.
    Damaged(Vec<u64>),
    /// Every page matched except the last, partial one. The package's bytes
    /// past the end of the file are unknown, so it is compared once fetched.
    TailUnknown,
    /// Files the Xbox app writes or rewrites itself.
    Skipped(&'static str),
}

/// The package header for `folder`: a `streaming` download keeps it as its
/// container prefix, and the Xbox app keeps the same bytes beside an installed
/// game's `Content` folder, in a file named after the content id.
fn header_path(folder: &Path) -> Option<(PathBuf, bool)> {
    let prefix = folder.join(crate::xbox::PREFIX_FILE);
    if prefix.is_file() {
        return Some((prefix, false));
    }
    let parent = folder.parent()?;
    let cache = std::fs::read_dir(parent).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.is_file() && p.extension().is_none() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| Uuid::try_parse(n).is_ok())
    })?;
    Some((cache, true))
}

fn target(root: &Path, name: &str) -> Option<PathBuf> {
    let relative = PathBuf::from(name.replace('\\', "/"));
    relative.components().all(|c| matches!(c, Component::Normal(_))).then(|| root.join(relative))
}

/// Compare one file with the package, page by page.
fn check(check: &PageCheck, path: &Path, file: &SegmentFile, done: &AtomicU64) -> State {
    let expected = match file.stored_length() {
        Ok(length) => length,
        Err(_) => return State::Skipped("invalid length in the package"),
    };
    let handle = match std::fs::File::open(path) {
        Ok(handle) => handle,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return State::Missing,
        // The Xbox app's encrypted executables deny every plain read.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied && file.keep_encrypted => {
            return State::Skipped("written by the Xbox app");
        }
        Err(_) => return State::Skipped("could not be read"),
    };
    match handle.metadata() {
        Ok(metadata) if metadata.len() != expected => return State::WrongSize(metadata.len()),
        Ok(_) => {}
        Err(_) => return State::Skipped("could not be read"),
    }
    let mut reader = std::io::BufReader::with_capacity(1 << 20, handle);
    let (mut bad, mut unhashed) = (vec![], 0);
    let pages = expected.div_ceil(PAGE);
    for index in 0..pages {
        let mut page = [0u8; PAGE_SIZE];
        let length = (expected - index * PAGE).min(PAGE) as usize;
        if reader.read_exact(&mut page[..length]).is_err() {
            return State::Skipped("could not be read");
        }
        match check.matches(file, index, &page) {
            Some(true) => {}
            Some(false) => bad.push(index),
            None => unhashed += 1,
        }
        done.fetch_add(length as u64, Ordering::Relaxed);
    }
    if unhashed == pages && pages > 0 {
        return State::Skipped("the package has no hashes for it");
    }
    let partial = expected % PAGE != 0;
    match bad.as_slice() {
        [] => State::Ok,
        [last] if partial && *last == pages - 1 => State::TailUnknown,
        _ => State::Damaged(bad),
    }
}

/// Check every file on `threads` threads, reporting progress to stderr.
fn check_all(xvd: &XvdFile, key: [u8; 32], root: &Path, files: &[(String, SegmentFile)], skip: impl Fn(&str) -> Option<&'static str> + Sync, quiet: bool) -> Vec<State> {
    let total: u64 = files.iter().map(|(_, f)| f.length).sum();
    let done = AtomicU64::new(0);
    let next = AtomicUsize::new(0);
    let states = Mutex::new(vec![State::Ok; files.len()]);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let finished = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let page_check = xvd.page_check(key);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, file)) = files.get(i) else { break };
                    let state = match (skip(name), target(root, name)) {
                        (Some(reason), _) => State::Skipped(reason),
                        (None, None) => State::Skipped("unsafe path in the package"),
                        (None, Some(path)) => check(&page_check, &path, file, &done),
                    };
                    states.lock().unwrap()[i] = state;
                }
                finished.fetch_add(1, Ordering::Relaxed);
            });
        }
        let started = Instant::now();
        let mut shown = Instant::now();
        while finished.load(Ordering::Relaxed) < threads {
            std::thread::sleep(Duration::from_millis(200));
            if !quiet && shown.elapsed() >= Duration::from_secs(5) {
                let read = done.load(Ordering::Relaxed);
                let rate = read as f64 / started.elapsed().as_secs_f64() / 1_048_576.0;
                eprintln!("Checked {:.1} of {:.1} GB ({rate:.0} MB/s)", read as f64 / 1e9, total as f64 / 1e9);
                shown = Instant::now();
            }
        }
    });
    states.into_inner().unwrap()
}

/// Fetch `file` into `path` from the package, every page checked on the way.
/// Written beside the target first, so a failed fetch leaves the old file.
async fn fetch(xvd: &XvdFile, client: &reqwest::Client, url: &str, key: [u8; 32], path: &Path, file: &SegmentFile) -> Result<(), String> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".xodus-repair");
    let temp = PathBuf::from(temp);
    let result = async {
        let mut out = tokio::fs::File::create(&temp).await.map_err(|e| e.to_string())?;
        let content = SegmentFile { offset: file.offset, length: file.length, data_hashs: vec![], keep_encrypted: file.keep_encrypted };
        xvd.download_file_http(client, url, &mut out, &content, key, |_, _| {}, None, 0, |_| Ok(())).await.map_err(|e| e.to_string())?;
        out.flush().await.map_err(|e| e.to_string())?;
        drop(out);
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    }
    .await;
    if result.is_err() {
        std::fs::remove_file(&temp).ok();
    }
    result
}

/// Fetch only the damaged `pages` of a plain file and write them in place.
async fn fetch_pages(xvd: &XvdFile, client: &reqwest::Client, url: &str, key: [u8; 32], path: &Path, file: &SegmentFile, pages: &[u64]) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let mut out = std::fs::OpenOptions::new().write(true).open(path).map_err(|e| e.to_string())?;
    for &page in pages {
        let start = page * PAGE;
        let part = SegmentFile { offset: file.offset + start, length: (file.length - start).min(PAGE), data_hashs: vec![], keep_encrypted: false };
        let mut bytes = vec![];
        xvd.download_file_http(client, url, &mut bytes, &part, key, |_, _| {}, None, 0, |_| Ok(())).await.map_err(|e| e.to_string())?;
        out.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        out.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    out.sync_all().map_err(|e| e.to_string())
}

/// The bytes of the last, partial page of `file`, fetched and checked.
async fn fetch_tail(xvd: &XvdFile, client: &reqwest::Client, url: &str, key: [u8; 32], file: &SegmentFile) -> Result<Vec<u8>, String> {
    let last = file.length / PAGE * PAGE;
    let tail = SegmentFile { offset: file.offset + last, length: file.length - last, data_hashs: vec![], keep_encrypted: false };
    let mut bytes = vec![];
    xvd.download_file_http(client, url, &mut bytes, &tail, key, |_, _| {}, None, 0, |_| Ok(())).await.map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn describe(state: &State, file: &SegmentFile) -> String {
    match state {
        State::Ok => "ok".into(),
        State::Missing => "missing".into(),
        State::WrongSize(actual) => format!("wrong size ({actual} bytes, expected {})", file.stored_length().unwrap_or(file.length)),
        State::Damaged(pages) => format!("damaged ({} of {} pages)", pages.len(), file.length.div_ceil(PAGE)),
        State::TailUnknown => "last page unverified".into(),
        State::Skipped(reason) => format!("not checked: {reason}"),
    }
}

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    folder: String,
    header: Option<String>,
    market: Option<String>,
    repair: bool,
    json: bool,
) -> ExitCode {
    let root = PathBuf::from(&folder);
    let (header, installed) = match header {
        Some(path) => (PathBuf::from(path), false),
        None => match header_path(&root) {
            Some(found) => found,
            None => {
                eprintln!("No package header found for {folder}. Pass --header with a .xodus-streaming.msixvc from a download of the same version.");
                return ExitCode::FAILURE;
            }
        },
    };
    let xvd = match XvdFile::parse_file(header.to_string_lossy().into_owned()).await {
        Ok(xvd) => xvd,
        Err(error) => {
            eprintln!("Could not read the package header {}: {error}", header.display());
            return ExitCode::FAILURE;
        }
    };
    let listed = async {
        let mut file = tokio::fs::File::open(&header).await?;
        let mut files = HashMap::new();
        for (name, user) in xvd.parse_user_package_files(&mut file).await? {
            if name == "SegmentMetadata.bin" {
                files = xvd.parse_segment_metadata(&mut file, &user).await?;
            }
        }
        if files.is_empty() {
            files = xvd.parse_ntfs_segment_metadata(&mut file, false).await?;
        }
        Ok::<_, Box<dyn std::error::Error>>(files)
    };
    let mut files: Vec<(String, SegmentFile)> = match listed.await {
        Ok(files) if !files.is_empty() => files.into_iter().collect(),
        Ok(_) => {
            eprintln!("The package lists no files.");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("Could not read the package's file list: {error}");
            return ExitCode::FAILURE;
        }
    };
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let content_id = xvd.content_id().to_string();
    let key = match content_key(client, tokens, content_id.clone(), market).await {
        Ok(key) => key,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if !json {
        eprintln!("Checking {} files of {content_id} in {folder}", files.len());
    }
    // Gaming Services rewrites the manifest when it registers the install.
    let skip = |name: &str| (installed && name.eq_ignore_ascii_case("appxmanifest.xml")).then_some("rewritten by the Xbox app");
    let mut states = tokio::task::block_in_place(|| check_all(&xvd, key, &root, &files, skip, json));

    let mut url = None;
    let needs_network = states.iter().zip(&files).any(|(state, (_, file))| match state {
        State::TailUnknown => true,
        State::Missing | State::WrongSize(_) | State::Damaged(_) => repair && !(installed && file.keep_encrypted),
        _ => false,
    });
    if needs_network {
        match package_urls(client, tokens, content_id.clone()).await {
            Ok(urls) => url = Some(urls[0].clone()),
            Err(error) => eprintln!("Could not reach the package to finish the check: {error}"),
        }
    }
    let mut repaired = vec![];
    for (state, (name, file)) in states.iter_mut().zip(&files) {
        let (Some(url), Some(path)) = (url.as_deref(), target(&root, name)) else { continue };
        if *state == State::TailUnknown {
            let last = (file.length / PAGE * PAGE) as usize;
            *state = match (fetch_tail(&xvd, client, url, key, file).await, std::fs::read(&path)) {
                (Ok(tail), Ok(bytes)) if bytes[last..] == tail[..] => State::Ok,
                (Ok(_), Ok(_)) => State::Damaged(vec![file.length / PAGE]),
                (Err(error), _) => {
                    eprintln!("{name}: could not fetch its last page: {error}");
                    continue;
                }
                (_, Err(_)) => State::Skipped("could not be read"),
            };
        }
        let broken = matches!(state, State::Missing | State::WrongSize(_) | State::Damaged(_));
        if !repair || !broken {
            continue;
        }
        if installed && file.keep_encrypted {
            // Only Gaming Services may write these; see AGENTS.md.
            continue;
        }
        if !json {
            eprintln!("Fetching {name} again ({})", describe(state, file));
        }
        let fetched = match state {
            // Plain pages stand alone: fetch just those, not a 20 GB file.
            State::Damaged(pages) if !file.keep_encrypted => fetch_pages(&xvd, client, url, key, &path, file, pages).await,
            _ => fetch(&xvd, client, url, key, &path, file).await,
        };
        match fetched {
            Ok(()) => {
                let page_check = xvd.page_check(key);
                let after = check(&page_check, &path, file, &AtomicU64::new(0));
                if matches!(after, State::Ok | State::TailUnknown) {
                    repaired.push(name.clone());
                    *state = State::Ok;
                } else {
                    eprintln!("{name}: still {} after fetching it again", describe(&after, file));
                }
            }
            Err(error) => eprintln!("{name}: could not fetch it again: {error}"),
        }
    }

    let problems: Vec<_> = states
        .iter()
        .zip(&files)
        .filter(|(state, _)| !matches!(state, State::Ok | State::Skipped(_)))
        .map(|(state, (name, file))| (name.clone(), describe(state, file)))
        .collect();
    let skipped: Vec<_> = states
        .iter()
        .zip(&files)
        .filter(|(state, _)| matches!(state, State::Skipped(_)))
        .map(|(state, (name, file))| (name.clone(), describe(state, file)))
        .collect();
    let ok = states.iter().filter(|s| **s == State::Ok).count();
    if json {
        let list = |items: &[(String, String)]| items.iter().map(|(n, s)| serde_json::json!({"file": n, "state": s})).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::json!({
                "schema": 1, "contentId": content_id, "files": files.len(), "ok": ok,
                "problems": list(&problems), "skipped": list(&skipped), "repaired": repaired,
            })
        );
    } else {
        for (name, state) in &skipped {
            println!("{name}: {state}");
        }
        for name in &repaired {
            println!("{name}: fetched again, now matches the package");
        }
        for (name, state) in &problems {
            println!("{name}: {state}");
        }
        println!("{ok} of {} files match the package; {} with problems; {} not checked.", files.len(), problems.len(), skipped.len());
        if !problems.is_empty() && !repair {
            println!("Run again with --repair to fetch the damaged files again.");
        }
        if problems.iter().any(|(name, _)| files.iter().any(|(n, f)| n == name && f.keep_encrypted)) && installed {
            println!("Damaged executables are written by the Xbox app: use Verify and repair there.");
        }
    }
    if problems.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
