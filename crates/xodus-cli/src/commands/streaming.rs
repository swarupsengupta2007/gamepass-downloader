use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::vec;

use fs2::available_space;
use futures_util::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use msixvc::streaming;
use msixvc::xvd::{SegmentFile, XvdFile};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncRead;
use tokio::sync::mpsc::{Receiver, Sender};
use uuid::Uuid;
use xodus::tokens::TokenManager;

use crate::license::get_license;
use crate::package::get_packages;

#[path = "streaming_progress.rs"]
mod progress;
#[path = "streaming_resume.rs"]
mod resume;
#[path = "streaming_failure.rs"]
mod failure;
#[path = "streaming_split.rs"]
mod split;
#[path = "streaming_batch.rs"]
mod batch;

#[cfg(test)]
#[path = "streaming_preflight_tests.rs"]
mod preflight_tests;

struct Job {
    name: String,
    content: SegmentFile,
}

enum Work {
    File(Job),
    /// Small neighbouring files fetched with one request (`batch`).
    Batch(Vec<(String, SegmentFile)>),
}

impl Work {
    fn size(&self) -> u64 {
        match self {
            Work::File(job) => job.content.length,
            Work::Batch(files) => files.iter().map(|(_, f)| f.length).sum(),
        }
    }
}

fn pending_files<'a>(
    root: &Path,
    remote: &'a HashMap<String, SegmentFile>,
    local: &HashMap<String, SegmentFile>,
) -> std::io::Result<Vec<(&'a String, &'a SegmentFile)>> {
    let mut pending = Vec::new();
    for (name, file) in remote {
        let unchanged = local.get(name).is_some_and(|old| {
            !file.data_hashs.is_empty() && file.data_hashs == old.data_hashs
                && file.length == old.length && file.keep_encrypted == old.keep_encrypted
        });
        let needs_repair = if unchanged && file.keep_encrypted {
            // Matching package metadata alone does not prove that the on-disk
            // ciphertext still includes the full final page.
            let target = resume::target(root, name)?;
            match std::fs::metadata(target) {
                Ok(metadata) if metadata.is_file() => metadata.len() < file.stored_length()?,
                Ok(_) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "package image is not a regular file")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(error),
            }
        } else { false };
        if !unchanged || needs_repair { pending.push((name, file)); }
    }
    Ok(pending)
}

/// The package's content key from the account's license. A missing
/// entitlement is an error.
pub(crate) async fn content_key(
    client: &reqwest::Client,
    tokens: &TokenManager,
    content_id: String,
    market: Option<String>,
) -> Result<[u8; 32], String> {
    let (key, license) = get_license(client, tokens, content_id, market.unwrap_or("neutral".to_string()))
        .await
        .map_err(|error| error.to_string())?;
    if license.content_keys.len() != 1 {
        return Err(format!("unexpected number of content keys {}", license.content_keys.len()));
    }
    let (_, content_key) = license.content_keys.into_iter().next().expect("one key");
    content_key
        .unpack(&key)
        .map(|key| *key)
        .map_err(|_| "The content key could not be authenticated. Retry the installation or sign in again.".to_owned())
}

/// Every CDN location of the package, fastest first, with the order also
/// given to `msixvc` for refetching damaged pages.
pub(crate) async fn package_urls(client: &reqwest::Client, tokens: &TokenManager, content_id: String) -> Result<Vec<String>, String> {
    let package = get_packages(client, tokens, content_id).await.map_err(|error| error.to_string())?;
    let file = package.package_files.iter().find(|p| p.file_name.ends_with(".msixvc")).ok_or("No .msixvc file found")?;
    let urls: Vec<String> = ranked_roots(client, file).await.into_iter().map(|root| format!("{root}{}", file.relative_url)).collect();
    if urls.is_empty() {
        return Err("Microsoft returned no package download location.".to_owned());
    }
    msixvc::streaming_http::set_mirrors(urls.clone());
    Ok(urls)
}

/// Rank the CDN roots by how fast they serve this package right now. Edges
/// differ a lot: one can sit near 0 B/s while another runs at full line rate,
/// which is also why the Xbox app's own download stalls. Probes a 4 MiB range
/// from the middle of the package on every root, foreground and background, for
/// at most a few seconds each. Roots are reported by index only.
async fn ranked_roots(client: &reqwest::Client, file: &xodus::models::packagespc::PackageFile) -> Vec<String> {
    const PROBE: u64 = 4 * 1024 * 1024;
    let roots: Vec<&String> = file.cdn_root_paths.iter().chain(&file.background_cdn_root_paths).collect();
    if roots.len() < 2 {
        return roots.into_iter().cloned().collect();
    }
    let size = u64::try_from(file.file_size).unwrap_or(0);
    let start = (size / 2) & !0xFFF;
    let end = (start + PROBE).min(size.max(1)) - 1;
    let mut ranked: Vec<(f64, &String)> = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let begun = std::time::Instant::now();
        let mut received = 0u64;
        let probe = async {
            let mut response = client
                .get(format!("{root}{}", file.relative_url))
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            while let Ok(Some(chunk)) = response.chunk().await {
                received += chunk.len() as u64;
                if received >= PROBE {
                    break;
                }
            }
            Some(())
        };
        let finished = tokio::time::timeout(std::time::Duration::from_secs(6), probe).await;
        let rate = received as f64 / begun.elapsed().as_secs_f64().max(0.001);
        eprintln!(
            "CDN root {index}: {:.1} MB/s{}",
            rate / 1_048_576.0,
            if matches!(finished, Ok(Some(()))) { "" } else { " (incomplete)" }
        );
        ranked.push((rate, root));
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().map(|(_, root)| root.clone()).collect()
}

enum ProgressEvent {
    Started { id: usize, name: String, total: u64 },
    Advanced { id: usize, delta: u64 },
    Finished { id: usize },
    UpdateRemaining { name: String, total: u64 },
    UpdateStatus { name: String },
    Paused { paused: bool },
    Completed { success: bool },
}

pub async fn run(
    client: &reqwest::Client,
    tokens: &TokenManager,
    source: String,
    destination: String,
    try_skip_ntfs: bool,
    parallel: Option<usize>,
    market: Option<String>,
    progress_json: bool,
    control_stdin: bool,
    selected_content_id: Option<String>,
) -> ExitCode {
    if selected_content_id.is_some() && (Uuid::try_parse(&source).is_ok() || source.contains("://")) {
        eprintln!("Package selection requires a Store product ID");
        return ExitCode::FAILURE;
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(256);
    let pause = progress::controls(control_stdin, tx.clone());
    if source.starts_with("file://") {
        let fsrc = source.strip_prefix("file://").unwrap_or_default();
        let f = match File::open(fsrc).await {
            Ok(f) => f,
            Err(err) => {
                eprintln!("could not open {fsrc}: {err}");
                return ExitCode::FAILURE;
            }
        };
        let l = match f.metadata().await {
            Ok(metadata) => metadata.len(),
            Err(err) => {
                eprintln!("could not read metadata for {fsrc}: {err}");
                return ExitCode::FAILURE;
            }
        };
        return run_cli_reader(
            client,
            tokens,
            destination,
            try_skip_ntfs,
            parallel,
            market,
            f,
            l,
            &source,
            &tx,
            rx,
            progress_json,
            pause,
        )
        .await;
    } else {
        let vurl = if source.starts_with("http://") || source.starts_with("https://") {
            source
        } else {
            let content_id = if Uuid::try_parse(&source).is_err() {
                let region = market.clone().unwrap_or("US".into()).to_uppercase();
                let content_id_task = match crate::package_plan::resolve(client, &source.to_uppercase(), &region).await {
                    Ok(plan) => if selected_content_id.is_some() { plan.select(selected_content_id.as_deref()) }
                        else { plan.without_durable_selection().select(None) },
                    Err(error) => Err(error),
                };
                let Ok(content_id) = content_id_task else {
                    let Err(err) = content_id_task else {
                        eprintln!("Unknown Error");
                        return ExitCode::FAILURE;
                    };
                    eprintln!("{}", err);
                    return ExitCode::FAILURE;
                };
                content_id
            } else {
                source
            };
            match package_urls(client, tokens, content_id).await {
                Ok(urls) => urls[0].clone(),
                Err(error) => { eprintln!("{error}"); return ExitCode::FAILURE; }
            }
        };
        let url = &vurl;
        let mut pos = 0;
        let http_file = match streaming::HttpRead::open(
            client.clone(),
            url,
            Some(|c, _| {
                if tx
                    .try_send(ProgressEvent::Advanced {
                        id: usize::MAX,
                        delta: c - pos,
                    })
                    .is_ok()
                {
                    pos = c;
                }
            }),
        )
        .await {
            Ok(file) => file,
            Err(error) => { failure::report(&error); return ExitCode::FAILURE; }
        };
        let l = http_file.len();

        return run_cli_reader(
            client,
            tokens,
            destination,
            try_skip_ntfs,
            parallel,
            market,
            http_file,
            l,
            url,
            &tx,
            rx,
            progress_json,
            pause,
        )
        .await;
    }
}

async fn run_cli_reader<Reader>(
    client: &reqwest::Client,
    tokens: &TokenManager,
    destination: String,
    try_skip_ntfs: bool,
    parallel: Option<usize>,
    market: Option<String>,
    reader: Reader,
    l: u64,
    url: &str,
    tx: &Sender<ProgressEvent>,
    mut rx: Receiver<ProgressEvent>,
    progress_json: bool,
    pause: tokio::sync::watch::Receiver<bool>,
) -> ExitCode
where
    Reader: AsyncRead + Unpin,
{
    let status_root = std::path::PathBuf::from(&destination);
    let reporter = tokio::spawn(async move {
        if progress_json {
            progress::report(rx, status_root).await;
            return;
        }
        let multi_progress = MultiProgress::new();
        let total_progess = multi_progress.add(ProgressBar::new(l).with_style(
            ProgressStyle::with_template("{msg:30!} {bytes:>12}/{total_bytes:>12} {bytes_per_sec:>12} [{bar:40.cyan/blue}] {percent:>3}%").unwrap()
            .progress_chars("#>-")
        ));

        total_progess.set_message("Initializing");
        let mut bars: HashMap<usize, ProgressBar> = HashMap::new();

        while let Some(event) = rx.recv().await {
            match event {
                ProgressEvent::Started { id, name, total } => {
                    let cur_progess = multi_progress.add(ProgressBar::new(total).with_style(
                        ProgressStyle::with_template("{msg:30!} {bytes:>12}/{total_bytes:>12} {bytes_per_sec:>12} [{bar:40.cyan/blue}] {percent:>3}%").unwrap()
                        .progress_chars("#>-")
                    ));
                    cur_progess.set_message(name);
                    bars.insert(id, cur_progess);
                }
                ProgressEvent::Advanced { id, delta } => {
                    if let Some(bar) = bars.get(&id) {
                        bar.inc(delta);
                    }
                    total_progess.inc(delta);
                }
                ProgressEvent::Finished { id } => {
                    if let Some(bar) = bars.remove(&id) {
                        bar.finish_and_clear();
                    }
                }
                ProgressEvent::UpdateRemaining { name, total } => {
                    total_progess.set_message(name);
                    total_progess.set_length(total_progess.position() + total);
                }
                ProgressEvent::UpdateStatus { name } => {
                    total_progess.set_message(name);
                }
                ProgressEvent::Paused { .. } => {}
                ProgressEvent::Completed { .. } => break,
            }
        }

        total_progess.abandon();
    });
    // A partial or resumed folder must never look finished to `adopt`.
    let marker = std::path::Path::new(&destination).join(crate::xbox::COMPLETE_MARKER);
    std::fs::remove_file(&marker).ok();
    let code = run_reader(
        client,
        tokens,
        destination.clone(),
        try_skip_ntfs,
        parallel,
        market,
        reader,
        l,
        url,
        tx,
        pause,
    )
    .await;
    let code = if code == ExitCode::SUCCESS && !try_skip_ntfs {
        match crate::xbox::write_complete_marker(std::path::Path::new(&destination)) {
            Ok(()) => code,
            Err(error) => {
                eprintln!("could not record the finished download: {error}");
                ExitCode::FAILURE
            }
        }
    } else {
        code
    };
    tx.send(ProgressEvent::Completed {
        success: code == ExitCode::SUCCESS,
    })
    .await
    .ok();
    if reporter.await.is_err() {
        return ExitCode::FAILURE;
    }
    code
}

async fn run_reader<Reader>(
    client: &reqwest::Client,
    tokens: &TokenManager,
    destination: String,
    try_skip_ntfs: bool,
    parallel: Option<usize>,
    market: Option<String>,
    reader: Reader,
    l: u64,
    url: &str,
    tx: &Sender<ProgressEvent>,
    pause: tokio::sync::watch::Receiver<bool>,
) -> ExitCode
where
    Reader: AsyncRead + Unpin,
{
    let out: &Path = Path::new(&destination);

    if let Err(error) = std::fs::create_dir_all(out) {
        failure::report(&error); return ExitCode::FAILURE;
    }
    let _folder_lock = match resume::lock(out) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("Cannot start this download: {error}");
            return ExitCode::FAILURE;
        }
    };

    let cache_path = out.join(".xodus-streaming-tmp.msixvc");
    let final_path = out.join(".xodus-streaming.msixvc");

    let mut remote_file = match streaming::PrefixCacheFile::new(reader, l, cache_path.clone()).await {
        Ok(file) => file,
        Err(error) => { failure::report(&error); return ExitCode::FAILURE; }
    };
    let remote_xvd = match XvdFile::parse(&mut remote_file).await {
        Ok(xvd) => xvd,
        Err(error) => { failure::report_metadata(error.as_ref()); return ExitCode::FAILURE; }
    };
    let mut rfiles: HashMap<String, SegmentFile> = HashMap::new();
    let mut lfiles: HashMap<String, SegmentFile> = HashMap::new();

    let files = match remote_xvd
        .parse_user_package_files(&mut remote_file)
        .await
    {
        Ok(files) => files,
        Err(error) => {
            failure::report_metadata(error.as_ref());
            return ExitCode::FAILURE;
        }
    };
    for (k, v) in &files {
        if k == "SegmentMetadata.bin" {
            let sfiles = match remote_xvd
                .parse_segment_metadata(&mut remote_file, v)
                .await {
                Ok(files) => files,
                Err(error) => { failure::report_metadata(error.as_ref()); return ExitCode::FAILURE; }
            };
            rfiles = sfiles;
        }
    }

    if !try_skip_ntfs || rfiles.is_empty() {
        tx.send(ProgressEvent::UpdateStatus {
            name: "Downloading ntfs...".to_owned(),
        })
        .await
        .ok();
        let sfiles = match remote_xvd
            .parse_ntfs_segment_metadata(&mut remote_file, !rfiles.is_empty())
            .await {
            Ok(files) => files,
            Err(error) => { failure::report_metadata(error.as_ref()); return ExitCode::FAILURE; }
        };
        rfiles.extend(sfiles);
    }

    let file = OpenOptions::new()
        .read(true)
        .open(final_path.to_owned())
        .await
        .ok();

    if let Some(mut file) = file {
        let xvd = match XvdFile::parse(&mut file).await {
            Ok(xvd) => xvd,
            Err(error) => { failure::report_metadata(error.as_ref()); return ExitCode::FAILURE; }
        };

        let files = match xvd.parse_user_package_files(&mut file).await {
            Ok(files) => files,
            Err(error) => {
                failure::report_metadata(error.as_ref());
                return ExitCode::FAILURE;
            }
        };
        for (k, v) in &files {
            if k == "SegmentMetadata.bin" {
                let sfiles = match xvd.parse_segment_metadata(&mut file, v).await {
                    Ok(files) => files,
                    Err(error) => { failure::report_metadata(error.as_ref()); return ExitCode::FAILURE; }
                };
                lfiles = sfiles;
            }
        }

        if let Ok(sfiles) = xvd
            .parse_ntfs_segment_metadata(&mut file, !lfiles.is_empty())
            .await
        {
            lfiles.extend(sfiles);
        }
    }

    let full_key = match content_key(client, tokens, remote_xvd.content_id().to_string(), market).await {
        Ok(key) => key,
        Err(error) => { eprintln!("{error}"); return ExitCode::FAILURE; }
    };
    let full_key = &full_key;

    let mut pending = match pending_files(out, &rfiles, &lfiles) {
        Ok(files) => files,
        Err(error) => { eprintln!("Unable to inspect installed package files: {error}"); return ExitCode::FAILURE; }
    };
    pending.sort_by(|a, b| b.1.length.cmp(&a.1.length).then_with(|| a.0.cmp(b.0)));
    let total_size: u64 = pending
        .iter()
        .map(|(_, v)| v.length)
        .sum();

    let journal = match resume::Journal::open(out, &remote_xvd.resume_identity(), &rfiles) {
        Ok(journal) => std::sync::Arc::new(std::sync::Mutex::new(journal)),
        Err(error) => {
            eprintln!("Unable to prepare download checkpoints: {error}");
            return ExitCode::FAILURE;
        }
    };
    let saved: u64 = pending
        .iter()
        .map(|(name, file)| journal.lock().unwrap().offset(name, file.length))
        .sum();
    let required_free_space = total_size.saturating_sub(saved);
    let available_free_space = match available_space(out) {
        Ok(space) => space,
        Err(err) => {
            eprintln!(
                "failed to determine available space for {}: {}",
                out.display(),
                err
            );
            return ExitCode::FAILURE;
        }
    };

    if available_free_space < required_free_space {
        eprintln!(
            "not enough free disk space on {}: need {} bytes, have {} bytes (files: {})",
            out.display(),
            required_free_space,
            available_free_space,
            total_size
        );
        return ExitCode::FAILURE;
    }

    tx.send(ProgressEvent::UpdateRemaining {
        name: "Downloading".to_owned(),
        total: total_size,
    })
    .await
    .ok();

    // Small files not yet downloaded go in runs of neighbours, one request
    // each (`batch`); a file left alone in its run downloads as usual.
    let owned = |n: &String, v: &SegmentFile| {
        (n.clone(), SegmentFile { offset: v.offset, length: v.length, data_hashs: vec![], keep_encrypted: v.keep_encrypted })
    };
    let (small, single): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(name, file)| {
        !url.starts_with("file://") && batch::fits(file) && journal.lock().unwrap().offset(name, file.length) != file.length
    });
    let mut work: Vec<Work> = single.into_iter().map(|(n, v)| owned(n, v)).map(|(name, content)| Work::File(Job { name, content })).collect();
    for mut run in batch::runs(small.into_iter().map(|(n, v)| owned(n, v)).collect()) {
        work.push(match run.len() {
            1 => {
                let (name, content) = run.remove(0);
                Work::File(Job { name, content })
            }
            _ => Work::Batch(run),
        });
    }
    // Starting the largest first leaves small work for the end, so all
    // parallel streams stay busy until the tail. Large files also download in
    // several parts at once (`split`), so the last big file is not one stream.
    work.sort_by_key(|w| std::cmp::Reverse(w.size()));

    let remote_xvd_ref = &remote_xvd;
    let (failed, failure_signal) = tokio::sync::watch::channel(false);
    stream::iter(work.into_iter().enumerate())
    .for_each_concurrent(parallel.unwrap_or(4).max(1), |(id, work)| {
        let tx = tx.clone();
        let client = client.clone();
        let mut pause = pause.clone();
        let journal = journal.clone();
        let failed = failed.clone();
        let failure_signal = failure_signal.clone();
        async move {
            if *failed.borrow() { return; }
            let prepared = async {
                pause.wait_for(|paused| !*paused).await.ok();
                Ok(())
            };
            if failure::run_or_cancel(failure_signal.clone(), prepared).await.is_err() { return; }
            let job = match work {
                Work::File(job) => job,
                Work::Batch(files) => {
                    let total = files.iter().map(|(_, f)| f.length).sum();
                    tx.send(ProgressEvent::Started { id, name: format!("{} small files", files.len()), total }).await.ok();
                    let mut lp = 0;
                    let operation = batch::download(
                        remote_xvd_ref,
                        &client,
                        url,
                        out,
                        &files,
                        *full_key,
                        pause,
                        |pos| {
                            if tx.try_send(ProgressEvent::Advanced { id, delta: pos - lp }).is_ok() {
                                lp = pos;
                            }
                        },
                        |name, length| journal.lock().unwrap().checkpoint(name, length),
                    );
                    match failure::run_or_cancel(failure_signal, operation).await {
                        Ok(()) => { tx.send(ProgressEvent::Finished { id }).await.ok(); }
                        Err(error) => { if !failed.send_replace(true) { failure::report(error.as_ref()); } }
                    }
                    return;
                }
            };
            let opened = async {
                let target_file = resume::target(out, &job.name)?;
                if let Some(folder) = target_file.parent() { std::fs::create_dir_all(folder)?; }
                Ok::<_,failure::Error>(OpenOptions::new().create(true).write(true).truncate(false).open(&target_file).await?)
            }.await;
            let mut fout = match opened {
                Ok(file) => file,
                Err(error) => { if !failed.send_replace(true) { failure::report(error.as_ref()); } return; }
            };
            let operation = async {
            let mut offset = journal
                .lock()
                .unwrap()
                .offset(&job.name, job.content.length);
            if url.starts_with("file://") && offset != job.content.length {
                offset = 0;
            }
            offset = resume::prepare_output(&mut fout, offset, &job.content).await?;
            let mut lp = offset;

            let progress = |pos, _| {
                if tx
                    .try_send(ProgressEvent::Advanced {
                        id,
                        delta: pos - lp,
                    })
                    .is_ok()
                {
                    lp = pos;
                }
            };
            let path = job.name.to_owned();
            let shown = if path.chars().count() > 30 {
                let tail: String = path.chars().rev().take(27).collect::<Vec<_>>().into_iter().rev().collect();
                format!("...{tail}")
            } else {
                path.clone()
            };
            tx.send(ProgressEvent::Started {
                id,
                name: shown,
                total: job.content.length,
            })
            .await
            .ok();

            tx.send(ProgressEvent::Advanced { id, delta: offset })
                .await
                .ok();
            if offset == job.content.length {
                return Ok::<(),failure::Error>(());
            }
            if offset != 0 {
                journal.lock().unwrap().checkpoint(&job.name, offset)?;
            }

            if let Some(fpath) = url.strip_prefix("file://") {
                let mut i = File::open(&fpath).await?;
                remote_xvd_ref
                    .extract_file(&mut i, &mut fout, &job.content, *full_key, progress)
                    .await?;
            } else if !job.content.keep_encrypted && job.content.length - offset >= split::SPLIT_FROM {
                let target = resume::target(out, &job.name)?;
                split::download(
                    remote_xvd_ref,
                    &client,
                    url,
                    &target,
                    &job.content,
                    *full_key,
                    offset,
                    pause,
                    progress,
                    |position| journal.lock().unwrap().checkpoint(&job.name, position),
                )
                .await?;
            } else {
                remote_xvd_ref
                    .download_file_http(
                        &client,
                        url,
                        &mut fout,
                        &job.content,
                        *full_key,
                        progress,
                        Some(pause),
                        offset,
                        |position| journal.lock().unwrap().checkpoint(&job.name, position),
                    )
                    .await?;
            }
            Ok(())
            };
            let result = failure::run_or_cancel(failure_signal, operation).await;
            let result = failure::settle(&mut fout, result, || journal.lock().unwrap().checkpoint(&job.name,job.content.length)).await;
            match result {
                Ok(()) => { tx.send(ProgressEvent::Finished { id }).await.ok(); }
                Err(error) => { if !failed.send_replace(true) { failure::report(error.as_ref()); } }
            }
        }
    })
    .await;

    if let Err(error) = journal.lock().unwrap().commit() {
        eprintln!("Unable to save download checkpoints: {error}");
        return ExitCode::FAILURE;
    }
    if *failed.borrow() { return ExitCode::FAILURE; }
    tx.send(ProgressEvent::UpdateStatus {
        name: "Finalizing".to_owned(),
    })
    .await
    .ok();
    if let Err(error) = failure::finalize(&cache_path, &final_path, false) {
        failure::report(&error);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
