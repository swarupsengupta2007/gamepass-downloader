//! Synthetic ciphertext only. No account, license, game files or executable runs.
use super::*;
use crate::crypt::encrypt_page_xts;
use std::io::Cursor;

fn fixture(length: u64) -> (XvdFile, SegmentFile, [u8; 32], Vec<u8>, Vec<u8>) {
    let content = SegmentFile { offset: 0, length, keep_encrypted: true, data_hashs: vec![] };
    let stored = content.stored_length().unwrap() as usize;
    let mut bytes = XvdHeader::buffer();
    bytes[512..520].copy_from_slice(b"msft-xvd");
    bytes[528..536].copy_from_slice(&116444736000000000u64.to_le_bytes());
    bytes[652..656].copy_from_slice(&4u32.to_le_bytes());
    let header = XvdHeader::try_from_array(&bytes).unwrap();
    let layout = header.layout();
    let region = XvcRegionId::Other(7);
    let id = Uuid::nil();
    let units: Vec<u32> = (0..stored/PAGE_SIZE).map(|i| (i * 7 + 17) as u32).collect();
    let xvd = XvdFile { header, layout, encrypted_section_infos: vec![EncryptedSectionInfo {
        section_offset: 0, section_length: stored as u64, header_id: region, vduid: id,
        data_units: Some(units.clone()), first_segment_index: 0, data_hashs: vec![],
    }] };
    let key: [u8;32] = std::array::from_fn(|i| i as u8 + 1);
    let tweak_key: [u8;16] = key[..16].try_into().unwrap();
    let data_key: [u8;16] = key[16..].try_into().unwrap();
    let tweak_cipher = Aes128Enc::new((&tweak_key).into());
    let data_cipher = Aes128Enc::new((&data_key).into());
    let plain: Vec<u8> = (0..stored).map(|i| (i % 251) as u8).collect();
    let mut encrypted = plain.clone();
    for (i, page) in encrypted.chunks_exact_mut(PAGE_SIZE).enumerate() {
        encrypt_page_xts(page.try_into().unwrap(), TweakGenerator::new(region,id).with_data_unit(units[i]), &tweak_cipher, &data_cipher);
    }
    (xvd, content, key, plain[..length as usize].to_vec(), encrypted)
}

#[test]
fn streaming_stored_length_distinguishes_plain_and_ciphertext_and_rejects_overflow() {
    for (length, stored) in [(0,0), (1,4096), (4095,4096), (4096,4096), (4097,8192), (8192,8192)] {
        let mut file = SegmentFile { offset: 0, length, keep_encrypted: true, data_hashs: vec![] };
        assert_eq!(file.stored_length().unwrap(), stored);
        file.keep_encrypted = false;
        assert_eq!(file.stored_length().unwrap(), length);
    }
    let mut file = SegmentFile { offset: 0, length: u64::MAX, keep_encrypted: true, data_hashs: vec![] };
    assert!(file.stored_length().is_err());
    file.keep_encrypted = false;
    assert_eq!(file.stored_length().unwrap(), u64::MAX);
}

#[tokio::test]
async fn streaming_encrypted_download_and_partial_resume_keep_exact_ciphertext_for_launch_decryption() {
    for length in [1u64, 4095, 4096, 4097, 5000, 8192, 8193] {
        for initial in [0u64, 4096].into_iter().filter(|&n| n < length) {
            let (xvd, content, key, plain, ciphertext) = fixture(length);
            let stored = ciphertext.len();
            let body = ciphertext.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![];
                while !request.ends_with(b"\r\n\r\n") { request.push(socket.read_u8().await.unwrap()); assert!(request.len()<8192); }
                assert!(String::from_utf8(request).unwrap().to_ascii_lowercase().contains(&format!("range: bytes={initial}-{}\r\n", stored-1)));
                socket.write_all(format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {initial}-{}/{stored}\r\nConnection: close\r\n\r\n",stored-initial as usize,stored-1).as_bytes()).await.unwrap();
                for chunk in body[initial as usize..].chunks(137) { socket.write_all(chunk).await.unwrap(); }
            });
            let mut downloaded = ciphertext[..initial as usize].to_vec();
            let mut committed = vec![];
            let client = reqwest::Client::new();
            tokio::time::timeout(std::time::Duration::from_secs(3), xvd.download_file_http(
                &client, &format!("http://{address}/synthetic"), &mut downloaded, &content, key,
                |_,_| {}, None, initial, |n| {committed.push(n); Ok(())},
            )).await.unwrap().unwrap();
            server.await.unwrap();
            assert_eq!(downloaded.len(), stored);
            assert_eq!(downloaded, ciphertext);
            assert_eq!(committed.last(), Some(&length));
            let mut image = vec![];
            xvd.mount_mem_fd(&mut Cursor::new(&downloaded), &mut image, &content, key, |_,_| {}).await.unwrap();
            assert_eq!(image.len(), length as usize);
            assert_eq!(image, plain);
        }
    }
}

/// Serves `body` to each Range request, damaging page 1 of the first
/// `damaged` responses. Returns the requested start offsets.
async fn damaging_server(body: Vec<u8>, damaged: usize) -> (String, tokio::task::JoinHandle<Vec<usize>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut starts = vec![];
        while let Ok(Ok((mut socket, _))) = tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await {
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") { request.push(socket.read_u8().await.unwrap()); }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            let start: usize = request.split("range: bytes=").nth(1).unwrap().split('-').next().unwrap().parse().unwrap();
            let mut served = body[start..].to_vec();
            if starts.len() < damaged && start <= PAGE_SIZE + 3000 {
                served[PAGE_SIZE + 3000 - start] ^= 0xFF;
            }
            starts.push(start);
            socket.write_all(format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{}\r\nConnection: close\r\n\r\n", served.len(), body.len() - 1, body.len()).as_bytes()).await.unwrap();
            socket.write_all(&served).await.ok();
        }
        starts
    });
    (format!("http://{address}/synthetic"), server)
}

#[tokio::test]
async fn streaming_damaged_page_is_fetched_again_and_never_written() {
    let (mut xvd, mut content, key, plain, ciphertext) = fixture(3 * 4096 + 5);
    xvd.encrypted_section_infos[0].data_hashs = ciphertext.chunks(PAGE_SIZE).map(|p| sha2::Sha256::digest(p)[..20].try_into().unwrap()).collect();
    content.keep_encrypted = false;
    let (url, server) = damaging_server(ciphertext.clone(), 1).await;
    let (mut downloaded, mut progress) = (vec![], vec![]);
    xvd.download_file_http(&reqwest::Client::new(), &url, &mut downloaded, &content, key, |p, _| progress.push(p), None, 0, |_| Ok(())).await.unwrap();
    assert_eq!(downloaded, plain);
    assert_eq!(server.await.unwrap(), [0, PAGE_SIZE], "refetched from the damaged page");
    assert!(progress.windows(2).all(|w| w[0] < w[1]), "progress never goes back");

    let (url, server) = damaging_server(ciphertext, usize::MAX).await;
    let (mut downloaded, mut committed) = (vec![], vec![]);
    let error = xvd.download_file_http(&reqwest::Client::new(), &url, &mut downloaded, &content, key, |_, _| {}, None, 0, |n| { committed.push(n); Ok(()) }).await.unwrap_err();
    assert_eq!(error.downcast_ref::<TransferError>(), Some(&TransferError::new("transfer-integrity", 0)));
    assert_eq!(downloaded, plain[..PAGE_SIZE], "only the checked page was written");
    assert_eq!(committed, [PAGE_SIZE as u64]);
    assert_eq!(server.await.unwrap().len(), 5);
}

#[test]
fn page_check_matches_plain_and_kept_encrypted_pages_and_catches_damage() {
    let (mut xvd, mut content, key, plain, ciphertext) = fixture(2 * 4096 + 5);
    xvd.encrypted_section_infos[0].data_hashs = ciphertext.chunks(PAGE_SIZE).map(|p| sha2::Sha256::digest(p)[..20].try_into().unwrap()).collect();
    let check = xvd.page_check(key);
    let page = |bytes: &[u8], i: usize| -> [u8; PAGE_SIZE] {
        let mut page = [0; PAGE_SIZE];
        let part = &bytes[i * PAGE_SIZE..bytes.len().min((i + 1) * PAGE_SIZE)];
        page[..part.len()].copy_from_slice(part);
        page
    };
    for i in 0..3 {
        assert_eq!(check.matches(&content, i, &page(&ciphertext, i as usize)), Some(true), "ciphertext page {i}");
    }
    assert_eq!(check.matches(&content, 3, &[0; PAGE_SIZE]), None, "past the hashed pages");
    content.keep_encrypted = false;
    // The fixture's plain bytes past the file end are not zero, so the last
    // page only matches with them.
    let full_plain: Vec<u8> = (0..3 * PAGE_SIZE).map(|i| (i % 251) as u8).collect();
    assert_eq!(&full_plain[..plain.len()], &plain[..]);
    for i in 0..3 {
        assert_eq!(check.matches(&content, i, &page(&full_plain, i as usize)), Some(true), "plain page {i}");
    }
    let mut damaged = page(&full_plain, 1);
    damaged[3296] ^= 1;
    assert_eq!(check.matches(&content, 1, &damaged), Some(false));
}

#[tokio::test]
async fn streaming_plain_output_remains_unpadded_and_short_ciphertext_returns_error() {
    let (xvd, mut content, key, plain, ciphertext) = fixture(5000);
    let mut image = vec![];
    let result = xvd.mount_mem_fd(&mut Cursor::new(&ciphertext[..5000]), &mut image, &content, key, |_,_| {}).await;
    assert_eq!(result.unwrap_err().downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::UnexpectedEof);
    content.keep_encrypted = false;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let(mut socket,_) = listener.accept().await.unwrap();
        let mut request=vec![];
        while !request.ends_with(b"\r\n\r\n") {request.push(socket.read_u8().await.unwrap());}
        socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8192\r\nContent-Range: bytes 0-8191/8192\r\nConnection: close\r\n\r\n").await.unwrap();
        socket.write_all(&ciphertext).await.unwrap();
    });
    let mut downloaded=vec![];
    let client=reqwest::Client::new();
    tokio::time::timeout(std::time::Duration::from_secs(3),xvd.download_file_http(
        &client,&format!("http://{address}/synthetic"),&mut downloaded,&content,key,|_,_|{},None,0,|_|Ok(()),
    )).await.unwrap().unwrap();
    server.await.unwrap();
    assert_eq!(downloaded.len(),5000);
    assert_eq!(downloaded,plain);
}
