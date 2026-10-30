//! Print every file in a local msixvc prefix: XVD offset, length, and whether
//! the package keeps it encrypted on disk. Usage: xvd_segments <msixvc> [--encrypted]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: xvd_segments <msixvc> [--encrypted]");
    let only_encrypted = std::env::args().any(|a| a == "--encrypted");
    let xvd = msixvc::xvd::XvdFile::parse_file(path.clone()).await?;
    let mut f = tokio::fs::File::open(&path).await?;
    let mut files = std::collections::HashMap::new();
    for (name, user) in xvd.parse_user_package_files(&mut f).await? {
        if name == "SegmentMetadata.bin" {
            files = xvd.parse_segment_metadata(&mut f, &user).await?;
        }
    }
    if files.is_empty() {
        files = xvd.parse_ntfs_segment_metadata(&mut f, false).await?;
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by_key(|(_, s)| s.offset);
    for (name, s) in files {
        if only_encrypted && !s.keep_encrypted {
            continue;
        }
        println!("{:>14} {:>12} {} {name}", s.offset, s.length, if s.keep_encrypted { "E" } else { "-" });
    }
    Ok(())
}
