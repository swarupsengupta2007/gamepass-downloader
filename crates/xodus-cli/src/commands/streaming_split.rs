//! Download one large file over several HTTP range requests at once. The CDN
//! serves each connection at its own rate, at times 1 MB/s or less, so a
//! 30 GB file over one stream outlasts everything else in the package.
//!
//! The file is cut into chunks that the connections take in order as they
//! finish their last one. A fixed share per connection left the whole
//! download waiting on the slowest connection's share at the end; now only
//! the last few chunks are in flight then.

use std::cell::{Cell, RefCell};
use std::io::SeekFrom;
use std::path::Path;

use futures_util::future::try_join_all;
use msixvc::layout::PAGE_SIZE;
use msixvc::xvd::{SegmentFile, XvdFile};
use tokio::fs::OpenOptions;
use tokio::io::AsyncSeekExt;

/// Files with at least this much left are split.
pub(super) const SPLIT_FROM: u64 = 256 * 1024 * 1024;
const CHUNK: u64 = 16 * 1024 * 1024;
const CONNECTIONS: usize = 8;

/// `[start, end)` byte ranges left to fetch from `offset`, which is on a page.
/// Every chunk but the last is a whole number of pages, so each decrypts on
/// its own.
pub(super) fn parts(offset: u64, length: u64) -> Vec<(u64, u64)> {
    const { assert!(CHUNK % PAGE_SIZE as u64 == 0) };
    (offset..length).step_by(CHUNK as usize).map(|start| (start, (start + CHUNK).min(length))).collect()
}

/// Bytes written from the file start without a gap: whole leading parts plus
/// the first unfinished one. Only this prefix is safe to record for resume.
fn contiguous(offset: u64, parts: &[(u64, u64)], written: &[Cell<u64>]) -> u64 {
    let mut end = offset;
    for (&(start, stop), done) in parts.iter().zip(written) {
        end = start + done.get();
        if end < stop {
            break;
        }
    }
    end
}

/// Fetch `content` from `offset` to its end into `target`, which `streaming`
/// has already opened and trimmed to `offset`. `checkpoint` gets the gap-free
/// prefix, so an interrupted run resumes from there and refetches the rest.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download(
    xvd: &XvdFile,
    client: &reqwest::Client,
    url: &str,
    target: &Path,
    content: &SegmentFile,
    key: [u8; 32],
    offset: u64,
    pause: tokio::sync::watch::Receiver<bool>,
    progress: impl FnMut(u64, u64),
    checkpoint: impl FnMut(u64) -> std::io::Result<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let parts = parts(offset, content.length);
    let cells = || -> Vec<Cell<u64>> { parts.iter().map(|_| Cell::new(0)).collect() };
    let (fetched, written, active, done) = (cells(), cells(), cells(), cells());
    let (recorded, next, total) = (Cell::new(offset), Cell::new(0), Cell::new(offset));
    let finished = tokio::sync::Notify::new();
    let (progress, checkpoint) = (RefCell::new(progress), RefCell::new(checkpoint));
    let (parts, fetched, written, active, done) = (&parts, &fetched, &written, &active, &done);
    let (recorded, next, total, finished) = (&recorded, &next, &total, &finished);
    let (progress, checkpoint, pause) = (&progress, &checkpoint, &pause);
    let left = |i: usize| parts[i].1 - parts[i].0 - fetched[i].get();
    let connections = (0..CONNECTIONS.min(parts.len())).map(|_| async move {
        let mut out = OpenOptions::new().write(true).open(target).await?;
        loop {
            let i = if next.get() < parts.len() {
                next.set(next.get() + 1);
                next.get() - 1
            } else {
                // Nothing left to start: take over the unfinished chunk with
                // the most left, racing the connection that has it. Pages are
                // verified, so both write the same bytes; the first to finish
                // wins and the other is dropped. The takeover starts at the
                // chunk start, because the other may be dropped mid-write.
                let open = (0..parts.len()).filter(|&i| done[i].get() == 0 && active[i].get() == 1);
                match open.max_by_key(|&i| left(i)) {
                    Some(i) => i,
                    None => return Ok::<_, Box<dyn std::error::Error>>(()),
                }
            };
            let (start, end) = parts[i];
            out.seek(SeekFrom::Start(start)).await?;
            let part = SegmentFile {
                offset: content.offset + start,
                length: end - start,
                data_hashs: vec![],
                keep_encrypted: content.keep_encrypted,
            };
            active[i].set(active[i].get() + 1);
            let fetch = xvd.download_file_http(
                client,
                url,
                &mut out,
                &part,
                key,
                |pos, _| {
                    if pos > fetched[i].get() {
                        total.set(total.get() + pos - fetched[i].replace(pos));
                        (*progress.borrow_mut())(total.get(), content.length);
                    }
                },
                Some(pause.clone()),
                0,
                |pos| {
                    written[i].set(written[i].get().max(pos));
                    let prefix = contiguous(offset, parts, written);
                    if prefix > recorded.get() {
                        recorded.set(prefix);
                        (*checkpoint.borrow_mut())(prefix)?;
                    }
                    Ok(())
                },
            );
            let overtaken = async {
                loop {
                    let notified = finished.notified();
                    if done[i].get() == 1 {
                        break;
                    }
                    notified.await;
                }
            };
            let result = tokio::select! {
                result = fetch => Some(result),
                () = overtaken => None,
            };
            active[i].set(active[i].get() - 1);
            match result {
                Some(Ok(())) => {
                    done[i].set(1);
                    finished.notify_waiters();
                }
                // The other connection on this chunk carries on.
                Some(Err(_)) if active[i].get() > 0 => return Ok(()),
                Some(Err(error)) => return Err(error),
                None => {}
            }
        }
    });
    try_join_all(connections).await?;
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn splits_into_page_aligned_chunks_that_cover_the_rest_of_the_file() {
        for (offset, length) in [(0, 37_600 * MIB + 123), (4096 * 7, 300 * MIB), (0, 256 * MIB), (64 * MIB, 2_000 * MIB + 1)] {
            let parts = parts(offset, length);
            assert!(parts.len() > CONNECTIONS, "more chunks than connections, so fast ones take more");
            assert_eq!(parts[0].0, offset);
            assert_eq!(parts.last().unwrap().1, length);
            for pair in parts.windows(2) {
                assert_eq!(pair[0].1, pair[1].0, "no gaps or overlaps");
                assert_eq!(pair[0].1 % PAGE_SIZE as u64, 0, "chunks start on a page");
            }
        }
        assert_eq!(parts(0, 37_600 * MIB).len(), 2350);
    }

    /// Serves `body` to any number of Range requests until dropped, counting
    /// the requests.
    pub(in super::super) async fn range_server(
        body: std::sync::Arc<Vec<u8>>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        stalling_server(body, usize::MAX).await
    }

    /// Like `range_server`, but the first request starting at `stall` gets no
    /// answer for a minute, like a connection the CDN has stopped feeding.
    async fn stalling_server(
        body: std::sync::Arc<Vec<u8>>,
        stall: usize,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        let stalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = body.clone();
                let stalled = stalled.clone();
                tokio::spawn(async move {
                    let mut request = vec![];
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await.unwrap());
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    let range = request.split("range: bytes=").nth(1).unwrap().split("\r\n").next().unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end = end.parse::<usize>().unwrap().min(body.len() - 1);
                    if start == stall && !stalled.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                        end + 1 - start,
                        body.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body[start..=end]).await.unwrap();
                });
            }
        });
        (format!("http://{address}/package"), requests)
    }

    #[tokio::test]
    async fn parts_reassemble_the_exact_file_and_checkpoint_the_gap_free_prefix() {
        // An unencrypted region: bytes are copied as served.
        let length = 2 * SPLIT_FROM + 12_345;
        let body: Vec<u8> = (0..length + 8192).map(|n| (n % 251) as u8).collect();
        let (url, requests) = range_server(std::sync::Arc::new(body.clone())).await;
        let mut header = vec![0; 4096];
        header[512..520].copy_from_slice(b"msft-xvd");
        header[528..536].copy_from_slice(&116444736000000000u64.to_le_bytes());
        header[652..656].copy_from_slice(&4u32.to_le_bytes());
        let xvd = XvdFile::parse(std::io::Cursor::new(header)).await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("big.bin");
        // Resume from 2 pages in, as `streaming` would after trimming.
        let resume_at = 8192;
        std::fs::write(&target, &body[4096..4096 + resume_at as usize]).unwrap();
        let content = SegmentFile { offset: 4096, length, data_hashs: vec![], keep_encrypted: false };
        let (mut last_progress, mut checkpoints) = (0, vec![]);
        let (_, pause) = tokio::sync::watch::channel(false);
        download(
            &xvd, &reqwest::Client::new(), &url, &target, &content, [0; 32], resume_at, pause,
            |pos, _| {
                assert!(pos >= last_progress);
                last_progress = pos;
            },
            |pos| {
                checkpoints.push(pos);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), &body[4096..4096 + length as usize]);
        // Idle connections may also take over the last chunks in flight.
        let chunks = parts(resume_at, length).len();
        assert!((chunks..chunks + CONNECTIONS).contains(&requests.load(std::sync::atomic::Ordering::SeqCst)));
        assert_eq!(last_progress, length);
        assert_eq!(checkpoints.last(), Some(&length));
        assert!(checkpoints.windows(2).all(|w| w[0] < w[1]));
    }

    #[tokio::test]
    async fn an_idle_connection_takes_over_a_stalled_chunk() {
        let length = SPLIT_FROM + 5_000;
        let body: Vec<u8> = (0..length + 8192).map(|n| (n * 7 % 253) as u8).collect();
        // The first chunk's first answer stalls for a minute.
        let (url, requests) = stalling_server(std::sync::Arc::new(body.clone()), 4096).await;
        let mut header = vec![0; 4096];
        header[512..520].copy_from_slice(b"msft-xvd");
        header[528..536].copy_from_slice(&116444736000000000u64.to_le_bytes());
        header[652..656].copy_from_slice(&4u32.to_le_bytes());
        let xvd = XvdFile::parse(std::io::Cursor::new(header)).await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("big.bin");
        std::fs::write(&target, b"").unwrap();
        let content = SegmentFile { offset: 4096, length, data_hashs: vec![], keep_encrypted: false };
        let mut checkpoints = vec![];
        let (_, pause) = tokio::sync::watch::channel(false);
        let started = std::time::Instant::now();
        let client = reqwest::Client::new();
        let fetch = download(&xvd, &client, &url, &target, &content, [0; 32], 0, pause, |_, _| {}, |pos| {
            checkpoints.push(pos);
            Ok(())
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), fetch).await.expect("waited on the stalled chunk").unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        assert_eq!(std::fs::read(&target).unwrap(), &body[4096..4096 + length as usize]);
        assert!(requests.load(std::sync::atomic::Ordering::SeqCst) > parts(0, length).len(), "the stalled chunk was fetched twice");
        assert_eq!(checkpoints.last(), Some(&length), "the whole file is recorded once the takeover finished");
    }

    #[test]
    fn records_only_the_prefix_without_gaps() {
        let parts = [(100, 200), (200, 300), (300, 400)];
        let written: Vec<Cell<u64>> = [0, 0, 0].map(Cell::new).into();
        assert_eq!(contiguous(100, &parts, &written), 100);
        written[1].set(100);
        written[2].set(40);
        assert_eq!(contiguous(100, &parts, &written), 100, "a finished later part does not count");
        written[0].set(60);
        assert_eq!(contiguous(100, &parts, &written), 160);
        written[0].set(100);
        assert_eq!(contiguous(100, &parts, &written), 340);
        written[2].set(100);
        assert_eq!(contiguous(100, &parts, &written), 400);
    }
}
