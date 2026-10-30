//! Print the XVD layout of a local msixvc prefix, in bytes.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: xvd_layout <msixvc>");
    let xvd = msixvc::xvd::XvdFile::parse_file(path).await?;
    let l = xvd.layout();
    println!("content_id {} pduid {} version {:?}", xvd.content_id(), xvd.header().pduid, xvd.header().package_version);
    for (name, s) in [
        ("header", l.header),
        ("embedded_xvd", l.embedded_xvd),
        ("mutable_data", l.mutable_data),
        ("hash_tree", l.hash_tree),
        ("user_data", l.user_data),
        ("xvc_info", l.xvc_info),
        ("dynamic_header", l.dynamic_header),
        ("drive_data", l.drive_data),
    ] {
        let start = s.start.to_bytes().0;
        println!("{name:<16} start {start:>14} len {:>14} end {:>14}", s.len.0, start + s.len.to_page_count().to_bytes().0);
    }

    let mut tf = tokio::fs::File::open(std::env::args().nth(1).unwrap()).await?;
    for (name, f) in xvd.parse_user_package_files(&mut tf).await? {
        println!("user file {name} offset {} len {}", f.offset, f.length);
        if let (Some(smd), true) = (std::env::args().nth(2), name == "SegmentMetadata.bin") {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut blob = vec![0u8; f.length as usize];
            tf.seek(std::io::SeekFrom::Start(f.offset)).await?;
            tf.read_exact(&mut blob).await?;
            println!("  identical to {smd}: {}", std::fs::read(&smd)? == blob);
        }
    }

    use msixvc::models::xvd::{XvcInfo, XvcRegionHeader};
    use msixvc_common::parse::{BinaryParse, BinaryTryParse};
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(std::env::args().nth(1).unwrap())?;
    f.seek(SeekFrom::Start(l.xvc_info.start.to_bytes().0))?;
    let mut buf = XvcInfo::buffer();
    f.read_exact(&mut buf)?;
    let info = XvcInfo::from_array(&buf);
    println!("regions {} update_segments {} initial_play_offset {}", info.region_count, info.update_segment_count, info.initial_play_offset);
    let mut buf = XvcRegionHeader::buffer();
    for _ in 0..info.region_count {
        f.read_exact(&mut buf)?;
        let h = XvcRegionHeader::try_from_array(&buf)?;
        let desc = String::from_utf16_lossy(&h.description);
        println!(
            "region {:?} key {:?} flags {:?} offset {:>14} len {:>14} end {:>14} {}",
            h.region_id, h.key_id, h.flags, h.offset.to_bytes().0, h.length.to_bytes().0,
            h.offset.to_bytes().0 + h.length.to_bytes().0, desc.trim_end_matches('\0')
        );
    }
    Ok(())
}
