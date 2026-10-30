//! Fetch runs of small neighbouring files with one HTTP range request. Every
//! request costs a round trip however small it is, so the thousands of 9 KB
//! files at the end of a package crawl at tens of KB/s one request apiece.

use std::io::{self, Cursor, SeekFrom};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use msixvc::layout::PAGE_SIZE;
use msixvc::xvd::{SegmentFile, XvdFile};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

/// Files smaller than this are worth batching.
const SMALL_FILE: u64 = 1024 * 1024;
/// Bytes one request may span, gaps included. The run is held in memory.
const MAX_SPAN: u64 = 16 * 1024 * 1024;
/// Bytes of files not wanted (already downloaded) a run may skip over.
const MAX_GAP: u64 = 256 * 1024;

pub(super) fn fits(file: &SegmentFile) -> bool {
    file.length > 0 && file.length < SMALL_FILE
}

/// End of the pages holding `file`: what a range request has to cover.
fn end(file: &SegmentFile) -> u64 {
    file.offset + file.length.div_ceil(PAGE_SIZE as u64) * PAGE_SIZE as u64
}

/// Group files into runs one request can fetch, in package order.
pub(super) fn runs(mut files: Vec<(String, SegmentFile)>) -> Vec<Vec<(String, SegmentFile)>> {
    files.sort_by(|a, b| a.1.offset.cmp(&b.1.offset).then_with(|| a.0.cmp(&b.0)));
    let mut runs: Vec<Vec<(String, SegmentFile)>> = Vec::new();
    let mut span = (0, 0);
    for file in files {
        let joins = runs.last().is_some() && file.1.offset <= span.1 + MAX_GAP && end(&file.1).max(span.1) - span.0 <= MAX_SPAN;
        if joins {
            span.1 = span.1.max(end(&file.1));
            runs.last_mut().expect("checked above").push(file);
        } else {
            span = (file.1.offset, end(&file.1));
            runs.push(vec![file]);
        }
    }
    runs
}

/// The fetched span, seekable by package offset, so `extract_file` decrypts
/// each member exactly as it would from the whole package.
struct Window<'a> {
    base: u64,
    bytes: Cursor<&'a [u8]>,
}

impl AsyncRead for Window<'_> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.bytes).poll_read(cx, buf)
    }
}

impl AsyncSeek for Window<'_> {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let SeekFrom::Start(at) = position else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "batch reads seek by package offset"));
        };
        let at = at.checked_sub(self.base).ok_or_else(|| io::Error::other("read before the fetched range"))?;
        self.bytes.set_position(at);
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.base + self.bytes.position()))
    }
}

/// Fetch one run with a single range request, then write each member whole
/// under `root`. `progress` gets bytes of member files done; `completed` is
/// called once a member is written.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download(
    xvd: &XvdFile,
    client: &reqwest::Client,
    url: &str,
    root: &Path,
    files: &[(String, SegmentFile)],
    key: [u8; 32],
    pause: tokio::sync::watch::Receiver<bool>,
    mut progress: impl FnMut(u64),
    mut completed: impl FnMut(&str, u64) -> io::Result<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = files.iter().map(|(_, f)| f.offset).min().unwrap_or(0);
    let stop = files.iter().map(|(_, f)| end(f)).max().unwrap_or(start);
    let total: u64 = files.iter().map(|(_, f)| f.length).sum();
    // Kept encrypted, the span comes back as served; members are decrypted
    // (or not) one by one below, each by its own flag.
    let span = SegmentFile { offset: start, length: stop - start, data_hashs: vec![], keep_encrypted: true };
    let mut raw = Vec::with_capacity(span.length as usize);
    xvd.download_file_http(
        client,
        url,
        &mut raw,
        &span,
        key,
        |pos, length| progress(pos * total / length.max(1)),
        Some(pause),
        0,
        |_| Ok(()),
    )
    .await?;
    for (name, file) in files {
        let mut window = Window { base: start, bytes: Cursor::new(&raw) };
        let mut bytes = Vec::with_capacity(file.stored_length()? as usize);
        xvd.extract_file(&mut window, &mut bytes, file, key, |_, _| {}).await?;
        let target = super::resume::target(root, name)?;
        if let Some(folder) = target.parent() {
            tokio::fs::create_dir_all(folder).await?;
        }
        tokio::fs::write(&target, &bytes).await?;
        completed(name, file.length)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, offset: u64, length: u64) -> (String, SegmentFile) {
        (name.to_owned(), SegmentFile { offset, length, data_hashs: vec![], keep_encrypted: false })
    }

    fn names(runs: &[Vec<(String, SegmentFile)>]) -> Vec<Vec<&str>> {
        runs.iter().map(|run| run.iter().map(|(n, _)| n.as_str()).collect()).collect()
    }

    #[test]
    fn groups_neighbours_and_breaks_on_gaps_and_span() {
        let page = PAGE_SIZE as u64;
        let files = vec![
            file("c", 2 * page, 100),
            file("a", 0, 9000),
            // Past the gap limit from "c".
            file("far", 3 * page + MAX_GAP + page, 10),
            file("d", 3 * page + MAX_GAP + 2 * page, page),
        ];
        assert_eq!(names(&runs(files)), [vec!["a", "c"], vec!["far", "d"]]);

        let many: Vec<_> = (0..40).map(|i| file(&format!("{i:02}"), i * 512 * 1024, 500 * 1024)).collect();
        let runs = runs(many);
        assert_eq!(runs.len(), 2, "a run never spans more than MAX_SPAN");
        for run in &runs {
            assert!(end(&run.last().unwrap().1) - run[0].1.offset <= MAX_SPAN);
        }
    }

    #[tokio::test]
    async fn one_request_writes_every_member_and_skips_the_gaps() {
        let page = PAGE_SIZE as u64;
        let body: Vec<u8> = (0..64 * page).map(|n| (n % 251) as u8).collect();
        let (url, requests) = super::super::split::tests::range_server(std::sync::Arc::new(body.clone())).await;
        let mut header = vec![0; 4096];
        header[512..520].copy_from_slice(b"msft-xvd");
        header[528..536].copy_from_slice(&116444736000000000u64.to_le_bytes());
        header[652..656].copy_from_slice(&4u32.to_le_bytes());
        let xvd = XvdFile::parse(Cursor::new(header)).await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut members = vec![file("a.bin", 4 * page, 9000), file("dir\\b.bin", 7 * page, 1), file("c.bin", 20 * page, page)];
        // Kept encrypted: stored as whole pages, not decrypted.
        members.push(("game.exe".to_owned(), SegmentFile { offset: 22 * page, length: 5000, data_hashs: vec![], keep_encrypted: true }));
        let total: u64 = members.iter().map(|(_, f)| f.length).sum();
        let (_, pause) = tokio::sync::watch::channel(false);
        let (mut last, mut done) = (0, vec![]);
        download(
            &xvd, &reqwest::Client::new(), &url, root.path(), &members, [0; 32], pause,
            |pos| {
                assert!(pos >= last && pos <= total);
                last = pos;
            },
            |name, length| {
                done.push((name.to_owned(), length));
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(last, total);
        assert_eq!(done.len(), 4);
        let at = |offset: u64, length: u64| &body[offset as usize..(offset + length) as usize];
        assert_eq!(std::fs::read(root.path().join("a.bin")).unwrap(), at(4 * page, 9000));
        assert_eq!(std::fs::read(root.path().join("dir").join("b.bin")).unwrap(), at(7 * page, 1));
        assert_eq!(std::fs::read(root.path().join("c.bin")).unwrap(), at(20 * page, page));
        assert_eq!(std::fs::read(root.path().join("game.exe")).unwrap(), at(22 * page, 2 * page));
    }
}
