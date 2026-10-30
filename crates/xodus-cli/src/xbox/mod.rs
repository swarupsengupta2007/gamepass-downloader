//! Hand a finished `streaming` download to the official Xbox app.
//!
//! Gaming Services decides what is left to download from two records: the
//! queue item in `HKLM\SOFTWARE\Microsoft\GamingServices\StreamingCheckpoints`
//! and the per-chunk residency index (`<content id>.xvi`) beside the install's
//! `Content` folder. Resume streams every chunk the index marks missing and
//! never hashes `Content`, so adoption puts the files in place and marks the
//! chunks resident. The service then finalizes the install itself.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use msixvc::models::xvd::{XvcInfo, XvcRegionFlags, XvcRegionHeader, XvdHeader};
use msixvc_common::parse::{BinaryParse, BinaryTryParse};
use uuid::Uuid;

pub mod sidecars;
#[cfg(windows)]
pub mod system;

/// The container prefix `streaming` keeps beside the game files.
pub const PREFIX_FILE: &str = ".xodus-streaming.msixvc";
/// Written by `streaming` only after every file finished.
pub const COMPLETE_MARKER: &str = ".xodus-streaming-complete.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIdentity {
    pub content_id: Uuid,
    /// The XVD `pduid`; Gaming Services calls it the build id.
    pub build_id: Uuid,
    pub version: String,
    /// Length of the leading resident regions (header, metadata, filesystem
    /// metadata). Gaming Services keeps exactly these bytes as the header
    /// cache file named after the content id.
    pub resident_len: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub fn read_identity(prefix: &Path) -> io::Result<PackageIdentity> {
    let mut file = File::open(prefix)?;
    let mut buf = XvdHeader::buffer();
    file.read_exact(&mut buf)?;
    let header = XvdHeader::try_from_array(&buf).map_err(|e| invalid(e.to_string()))?;
    let layout = header.layout();
    file.seek(SeekFrom::Start(layout.xvc_info.start.to_bytes().0))?;
    let mut buf = XvcInfo::buffer();
    file.read_exact(&mut buf)?;
    let info = XvcInfo::from_array(&buf);
    let mut resident_len = 0;
    let mut buf = XvcRegionHeader::buffer();
    for _ in 0..info.region_count {
        file.read_exact(&mut buf)?;
        let region = XvcRegionHeader::try_from_array(&buf).map_err(|e| invalid(e.to_string()))?;
        let start = region.offset.to_bytes().0;
        if start != resident_len || !region.flags.contains(XvcRegionFlags::RESIDENT) {
            break;
        }
        resident_len = start + region.length.to_bytes().0;
    }
    if resident_len == 0 {
        return Err(invalid("package has no resident header regions"));
    }
    Ok(PackageIdentity {
        content_id: header.vduid,
        build_id: header.pduid,
        version: header.package_version.to_string(),
        resident_len,
    })
}

/// A file the package keeps encrypted on disk (`KEEP_ENCRYPTED_ON_DISK`, the
/// game executables). Gaming Services writes these itself and tags each with
/// a `$KERNEL.GAMING.ENCRYPTED` extended attribute that the `gameflt` driver
/// needs to decrypt it at launch. User mode cannot set `$KERNEL.` attributes,
/// so a copied file without it fails to start ("16-bit application").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFile {
    /// Relative to `Content`, with `\` separators.
    pub path: String,
    /// The 4 KiB XVD pages holding it.
    pub pages: std::ops::Range<u64>,
}

/// The files in the package that only Gaming Services can write, read from the
/// segment metadata in the downloaded package header.
pub fn service_written_files(prefix: &Path) -> io::Result<Vec<ServiceFile>> {
    use msixvc::xvd::XvdFile;
    let prefix = prefix.to_string_lossy().into_owned();
    // A runtime of its own: the NTFS parser bridges sync reads onto the runtime
    // with `block_in_place`, which deadlocks inside a nested `block_on`.
    let files = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
        let runtime = runtime.map_err(|e| e.to_string())?;
        runtime.block_on(async {
            let read = async {
                let xvd = XvdFile::parse_file(prefix.clone()).await?;
                let mut file = tokio::fs::File::open(&prefix).await?;
                let mut files = Default::default();
                for (name, user) in xvd.parse_user_package_files(&mut file).await? {
                    if name == "SegmentMetadata.bin" {
                        files = xvd.parse_segment_metadata(&mut file, &user).await?;
                    }
                }
                if files.is_empty() {
                    // Older packages (Ori) list files only in NTFS metadata,
                    // which has no flag; there every `.exe` is kept encrypted.
                    files = xvd.parse_ntfs_segment_metadata(&mut file, false).await?;
                }
                Ok::<_, Box<dyn std::error::Error>>(files)
            };
            read.await.map_err(|e| e.to_string())
        })
    })
    .join()
    .map_err(|_| "the file list reader crashed".to_owned())
    .and_then(|files| files)
    .map_err(|e| invalid(format!("could not read the package's file list: {e}")))?;
    if files.is_empty() {
        return Err(invalid("the package lists no files"));
    }
    let page = msixvc::layout::PAGE_SIZE as u64;
    let mut out: Vec<_> = files
        .into_iter()
        .filter(|(_, f)| f.keep_encrypted)
        .map(|(path, f)| {
            let start = f.offset / page;
            let path = path.replace('/', "\\").trim_start_matches('\\').to_owned();
            ServiceFile { path, pages: start..start + f.length.div_ceil(page).max(1) }
        })
        .collect();
    out.sort_by_key(|f| f.pages.start);
    Ok(out)
}

pub fn write_complete_marker(root: &Path) -> io::Result<()> {
    let id = read_identity(&root.join(PREFIX_FILE))?;
    let marker = serde_json::json!({
        "schema": 1,
        "contentId": id.content_id,
        "buildId": id.build_id,
        "version": id.version,
        "finishedAt": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(root.join(COMPLETE_MARKER), format!("{marker}\n"))
}

/// The identity of a download `streaming` finished, or why it is not usable.
pub fn finished_download(root: &Path) -> io::Result<PackageIdentity> {
    let marker = std::fs::read(root.join(COMPLETE_MARKER)).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => invalid("the download has not finished; run streaming to completion first"),
        _ => error,
    })?;
    let marker: serde_json::Value = serde_json::from_slice(&marker).map_err(|_| invalid("unreadable download marker"))?;
    let id = read_identity(&root.join(PREFIX_FILE))?;
    if marker["contentId"].as_str() != Some(id.content_id.to_string().as_str())
        || marker["buildId"].as_str() != Some(id.build_id.to_string().as_str())
    {
        return Err(invalid("the download changed after it finished; run streaming again"));
    }
    Ok(id)
}

/// Files `streaming` wrote, relative to `root`, with `\` separators. Its own
/// cache, journal, lock and marker files are left out.
pub fn downloaded_files(root: &Path) -> io::Result<Vec<(String, u64)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink() {
                return Err(invalid(format!("unexpected link in download: {}", path.display())));
            }
            if kind.is_dir() {
                walk(root, &path, out)?;
                continue;
            }
            let relative = path.strip_prefix(root).expect("walk stays under root");
            let name = relative.to_string_lossy().replace('/', "\\");
            if dir == root && (name.starts_with(".xodus") || name.starts_with(".nexus")) {
                continue;
            }
            out.push((name, entry.metadata()?.len()));
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}
