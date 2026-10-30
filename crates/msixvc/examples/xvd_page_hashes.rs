//! Check a file kept encrypted (stored as whole ciphertext pages) against the
//! package's per-page hashes. Usage: xvd_page_hashes <msixvc> <package path> <local file>
use sha2::{Digest, Sha256};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, prefix, name, local] = &args[..] else {
        panic!("usage: xvd_page_hashes <msixvc> <package path> <local file>");
    };
    let xvd = msixvc::xvd::XvdFile::parse_file(prefix.clone()).await?;
    let mut f = tokio::fs::File::open(prefix).await?;
    let mut files = std::collections::HashMap::new();
    for (n, user) in xvd.parse_user_package_files(&mut f).await? {
        if n == "SegmentMetadata.bin" {
            files = xvd.parse_segment_metadata(&mut f, &user).await?;
        }
    }
    if files.is_empty() {
        files = xvd.parse_ntfs_segment_metadata(&mut f, false).await?;
    }
    let file = files.get(name.as_str()).expect("file not in package");
    let bytes = std::fs::read(local)?;
    let (mut good, mut bad) = (0, 0);
    for (i, expected) in file.data_hashs.iter().enumerate() {
        let page = &bytes[i * 4096..(i + 1) * 4096];
        if Sha256::digest(page)[..20] == expected[..] { good += 1 } else { bad += 1 }
    }
    println!("{} pages hashed: {good} match, {bad} differ", file.data_hashs.len());
    Ok(())
}
