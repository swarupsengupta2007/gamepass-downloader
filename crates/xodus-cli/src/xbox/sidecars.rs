//! Gaming Services streaming sidecars beside an install's `Content` folder.
//!
//! - `<content id>.<build id>.xsp`: the streaming plan, one record per chunk.
//! - `<content id>.xvi` (`crdi-xvc`): residency index, one entry per plan record.
//! - `<content id>.smd`: segment metadata, one segment per game file.
//!
//! Offsets were read from finished and in-progress installs; the service writes
//! no checksum over `.xvi`.

use std::ops::Range;

use uuid::Uuid;

const XSP_MAGIC: &[u8; 8] = b"MS-XPFM ";
const XSP_MAGIC_AT: usize = 0x200;
const XSP_PAGE_SIZE: usize = 0x208;
const XSP_CONTENT_ID: usize = 0x210;
const XSP_BUILD_ID: usize = 0x230;
const XSP_RECORD_COUNT: usize = 0x27C;
const XSP_TOTAL_DOWNLOAD: usize = 0x2C8;
const XSP_DISK_SPACE: usize = 0x2D0;
const XSP_RECORD_LEN: usize = 16;
const XSP_COPY_FLAG: u32 = 0x8800_0000;
const XVD_PAGE: u64 = 4096;

const XVI_MAGIC: &[u8; 8] = b"crdi-xvc";
const XVI_CONTENT_ID: usize = 0x240;
const XVI_SEGMENT_COUNT: usize = 0x350;
const XVI_STREAMED: usize = 0x360;
const XVI_RESIDENT: usize = 0x378;
const XVI_ENTRY_COUNT: usize = 0x390;
/// A 2-bit-per-segment bitmap starts here; the entry table follows it.
const XVI_SEGMENT_BITMAP: usize = 0x4F2;
const XVI_NEW_DATA: [u8; 4] = [2, 0, 0, 1];
const XVI_COPY_DATA: [u8; 4] = [3, 0, 0, 1];

const SMD_MAGIC: &[u8; 4] = b" PFX";

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("truncated at 0x{at:X}"))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, String> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("truncated at 0x{at:X}"))
}

fn guid_at(b: &[u8], at: usize) -> Result<Uuid, String> {
    b.get(at..at + 16)
        .map(|s| Uuid::from_bytes_le(s.try_into().unwrap()))
        .ok_or_else(|| format!("truncated at 0x{at:X}"))
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub content_id: Uuid,
    pub build_id: Uuid,
    /// One flag per record: `true` copies data from a previous build.
    pub copy_records: Vec<bool>,
    /// The XVD pages each record covers (4 KiB pages from the package start).
    pub pages: Vec<Range<u64>>,
    pub total_download: u64,
    pub disk_space_required: u64,
}

pub fn parse_plan(b: &[u8]) -> Result<Plan, String> {
    if b.get(XSP_MAGIC_AT..XSP_MAGIC_AT + 8) != Some(XSP_MAGIC) {
        return Err("not a streaming plan (.xsp)".into());
    }
    let page = u32_at(b, XSP_PAGE_SIZE)? as usize;
    let count = u32_at(b, XSP_RECORD_COUNT)? as usize;
    let mut copy_records = Vec::with_capacity(count);
    let mut pages = Vec::with_capacity(count);
    for i in 0..count {
        let at = page + i * XSP_RECORD_LEN;
        copy_records.push(match u32_at(b, at + 4)? {
            0 => false,
            XSP_COPY_FLAG => true,
            flag => return Err(format!("plan record {i} has unknown flag 0x{flag:X}")),
        });
        let start = u32_at(b, at + 8)? as u64;
        pages.push(start..start + u32_at(b, at + 12)? as u64);
    }
    Ok(Plan {
        content_id: guid_at(b, XSP_CONTENT_ID)?,
        build_id: guid_at(b, XSP_BUILD_ID)?,
        copy_records,
        pages,
        total_download: u64_at(b, XSP_TOTAL_DOWNLOAD)?,
        disk_space_required: u64_at(b, XSP_DISK_SPACE)?,
    })
}

impl Plan {
    /// Records that cover any of `pages`.
    pub fn records_covering(&self, pages: &[Range<u64>]) -> Vec<usize> {
        (0..self.pages.len())
            .filter(|&i| pages.iter().any(|p| p.start < self.pages[i].end && self.pages[i].start < p.end))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residency {
    pub entries: usize,
    /// Entries not yet in their final resident state.
    pub missing: usize,
    /// `\??\D:\Xbox\<folder>\Content` as Gaming Services recorded it. The
    /// service writes it once it creates `Content`, so a freshly queued and
    /// paused install has none yet.
    pub content_path: Option<String>,
}

/// Bytes after the table that must be zero when no path is recorded yet.
const XVI_PATH_SPAN: usize = 0x220;
const XVI_PATH_PREFIX: &[u8] = b"\\\0?\0?\0\\\0";

/// The entry table follows a per-segment bitmap whose length is close to, but
/// not exactly, 2 bits per segment: 17 installs put it between 1 byte short of
/// and 13 bytes past `segments / 4`. The table is found from the data instead,
/// and an offset outside this window means the layout is not understood.
fn plausible_table_start(start: usize, segments: usize) -> bool {
    let low = XVI_SEGMENT_BITMAP + (segments / 4).saturating_sub(4);
    let high = XVI_SEGMENT_BITMAP + segments.div_ceil(4) + segments / 16 + 16;
    (low..=high).contains(&start)
}

/// Where the entry table starts. The recorded Content path follows the table
/// directly; before the service records it, the table is the first non-zero
/// data, because the package header (record 0) is always streamed first.
fn locate_table(xvi: &[u8], entries: usize) -> Result<usize, String> {
    let from = XVI_SEGMENT_BITMAP;
    let rest = xvi.get(from..).ok_or("residency index is truncated")?;
    if let Some(at) = rest.windows(XVI_PATH_PREFIX.len()).position(|w| w == XVI_PATH_PREFIX) {
        return (from + at).checked_sub(entries * 4).filter(|&s| s >= from).ok_or_else(|| {
            "unexpected residency index layout (Content path inside the entry table)".to_owned()
        });
    }
    rest.iter()
        .position(|&b| b != 0)
        .map(|at| from + at)
        .ok_or_else(|| "the Xbox app has not streamed anything for this install yet; resume it until the size moves, then pause".to_owned())
}

fn residency_table(xvi: &[u8], plan: &Plan) -> Result<(usize, usize, Option<String>), String> {
    if xvi.get(..8) != Some(XVI_MAGIC) {
        return Err("not a residency index (.xvi)".into());
    }
    let content_id = guid_at(xvi, XVI_CONTENT_ID)?;
    if content_id != plan.content_id {
        return Err(format!("residency index is for {content_id}, plan is for {}", plan.content_id));
    }
    let entries = u32_at(xvi, XVI_ENTRY_COUNT)? as usize;
    if entries != plan.copy_records.len() {
        return Err(format!("residency index has {entries} entries, plan has {} records", plan.copy_records.len()));
    }
    let start = locate_table(xvi, entries)?;
    if !plausible_table_start(start, u32_at(xvi, XVI_SEGMENT_COUNT)? as usize) {
        return Err(format!("unexpected residency index layout (entry table at 0x{start:X})"));
    }
    let tail = start + entries * 4;
    let table = xvi.get(start..tail).ok_or("residency index is truncated")?;
    // Missing and resident entries are `[0|2|3, 0, 0, 0|1]`; in-flight ones (for
    // example `C8 ..` or `19 02 ..`) carry more. A misplaced table shows up as
    // stray bytes after those states.
    if table.chunks_exact(4).any(|e| matches!(e[0], 0 | 2 | 3) && (e[1] != 0 || e[2] != 0 || e[3] > 1)) {
        return Err(format!("unexpected residency index layout (entry table at 0x{start:X})"));
    }
    let after = xvi.get(tail..tail + XVI_PATH_SPAN).ok_or("residency index is truncated")?;
    if after.iter().all(|&b| b == 0) {
        // Found as the first non-zero byte: it has to be a finished header chunk.
        if !matches!(table[..4], [2 | 3, 0, 0, 1]) {
            return Err(format!("unexpected residency index layout (entry table at 0x{start:X})"));
        }
        return Ok((start, entries, None));
    }
    let path_units: Vec<u16> = after
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    let content_path = String::from_utf16(&path_units).map_err(|_| "residency index path is not UTF-16")?;
    if !content_path.starts_with(r"\??\") || !content_path.to_ascii_lowercase().ends_with(r"\content") {
        return Err(format!("unexpected residency index layout (no Content path at 0x{tail:X})"));
    }
    Ok((start, entries, Some(content_path)))
}

fn wanted(copy: bool) -> [u8; 4] {
    if copy { XVI_COPY_DATA } else { XVI_NEW_DATA }
}

pub fn inspect_residency(xvi: &[u8], plan: &Plan) -> Result<Residency, String> {
    let (start, entries, content_path) = residency_table(xvi, plan)?;
    let missing = plan
        .copy_records
        .iter()
        .enumerate()
        .filter(|(i, copy)| xvi[start + i * 4..start + i * 4 + 4] != wanted(**copy))
        .count();
    Ok(Residency { entries, missing, content_path })
}

/// Put the index in the state it has right after the last chunk lands: every
/// entry resident, streamed and resident byte counts at their totals. The
/// completion markers stay as they are so the service runs its own finalization
/// (package registration, `WindowsApps` junction, folder rename).
///
/// Records listed in `left_missing` are reset to missing instead, and their
/// bytes left out of the counts, so Resume streams exactly those chunks.
///
/// `content_path` is recorded only when the index has none yet.
pub fn mark_resident(xvi: &mut [u8], plan: &Plan, content_path: &str, left_missing: &[usize]) -> Result<Residency, String> {
    let before = inspect_residency(xvi, plan)?;
    let (start, entries, _) = residency_table(xvi, plan)?;
    if before.content_path.is_none() {
        let units: Vec<u8> = content_path.encode_utf16().flat_map(u16::to_le_bytes).collect();
        if units.len() + 2 > XVI_PATH_SPAN {
            return Err("install path is too long for the residency index".into());
        }
        let tail = start + entries * 4;
        xvi[tail..tail + units.len()].copy_from_slice(&units);
    }
    let mut missing_bytes = 0;
    for (i, copy) in plan.copy_records.iter().enumerate() {
        let entry = if left_missing.contains(&i) {
            missing_bytes += (plan.pages[i].end - plan.pages[i].start) * XVD_PAGE;
            [0; 4]
        } else {
            wanted(*copy)
        };
        xvi[start + i * 4..start + i * 4 + 4].copy_from_slice(&entry);
    }
    let streamed = plan.total_download.saturating_sub(missing_bytes);
    let resident = plan.disk_space_required.saturating_sub(missing_bytes);
    xvi[XVI_STREAMED..XVI_STREAMED + 8].copy_from_slice(&streamed.to_le_bytes());
    xvi[XVI_RESIDENT..XVI_RESIDENT + 8].copy_from_slice(&resident.to_le_bytes());
    Ok(before)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Path relative to `Content`, with `\` separators.
    pub path: String,
    pub size: u64,
}

pub fn parse_segments(smd: &[u8]) -> Result<Vec<Segment>, String> {
    if smd.get(..4) != Some(SMD_MAGIC) {
        return Err("not segment metadata (.smd)".into());
    }
    let header_len = u32_at(smd, 12)? as usize;
    let count = u32_at(smd, 16)? as usize;
    let paths = header_len + count * 16;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = header_len + i * 16;
        let units = smd.get(at + 2..at + 4).ok_or("segment metadata is truncated")?;
        let path_len = u16::from_le_bytes([units[0], units[1]]) as usize;
        let path_at = paths + u32_at(smd, at + 4)? as usize;
        let size = u64_at(smd, at + 8)?;
        let raw = smd.get(path_at..path_at + path_len * 2).ok_or("segment path is truncated")?;
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let path = String::from_utf16(&units).map_err(|_| "segment path is not UTF-16")?;
        out.push(Segment { path: path.replace('/', "\\"), size });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CID: &str = "c4984363-e73f-404e-83da-5e26e0de4a6e";
    const BUILD: &str = "8d270559-f932-481f-8fbf-06250bfaa1ec";

    fn plan_bytes(records: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 0x1000 + records.len() * 16];
        b[XSP_MAGIC_AT..XSP_MAGIC_AT + 8].copy_from_slice(XSP_MAGIC);
        b[XSP_PAGE_SIZE..XSP_PAGE_SIZE + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        b[XSP_CONTENT_ID..XSP_CONTENT_ID + 16].copy_from_slice(&Uuid::parse_str(CID).unwrap().to_bytes_le());
        b[XSP_BUILD_ID..XSP_BUILD_ID + 16].copy_from_slice(&Uuid::parse_str(BUILD).unwrap().to_bytes_le());
        b[XSP_RECORD_COUNT..XSP_RECORD_COUNT + 4].copy_from_slice(&(records.len() as u32).to_le_bytes());
        b[XSP_TOTAL_DOWNLOAD..XSP_TOTAL_DOWNLOAD + 8].copy_from_slice(&1000u64.to_le_bytes());
        b[XSP_DISK_SPACE..XSP_DISK_SPACE + 8].copy_from_slice(&3000u64.to_le_bytes());
        for (i, flag) in records.iter().enumerate() {
            let at = 0x1000 + i * 16;
            b[at + 4..at + 8].copy_from_slice(&flag.to_le_bytes());
            // Record i covers pages 10i..10i+10.
            b[at + 8..at + 12].copy_from_slice(&(i as u32 * 10).to_le_bytes());
            b[at + 12..at + 16].copy_from_slice(&10u32.to_le_bytes());
        }
        b
    }

    /// Where Pentiment, Ori and Resonance put the table; other installs differ.
    fn table_start(segments: usize) -> usize {
        XVI_SEGMENT_BITMAP + (segments + 2).div_ceil(4)
    }

    fn xvi_bytes(segments: u32, entries: &[[u8; 4]]) -> Vec<u8> {
        xvi_at(segments, table_start(segments as usize), entries, true)
    }

    /// A queued and paused install: no Content path recorded yet.
    fn fresh_xvi_bytes(segments: u32, entries: &[[u8; 4]]) -> Vec<u8> {
        xvi_at(segments, table_start(segments as usize), entries, false)
    }

    fn xvi_at(segments: u32, start: usize, entries: &[[u8; 4]], with_path: bool) -> Vec<u8> {
        let mut b = vec![0u8; 0x5000];
        b[..8].copy_from_slice(XVI_MAGIC);
        b[XVI_CONTENT_ID..XVI_CONTENT_ID + 16].copy_from_slice(&Uuid::parse_str(CID).unwrap().to_bytes_le());
        b[XVI_SEGMENT_COUNT..XVI_SEGMENT_COUNT + 4].copy_from_slice(&segments.to_le_bytes());
        b[XVI_ENTRY_COUNT..XVI_ENTRY_COUNT + 4].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        b[XVI_STREAMED..XVI_STREAMED + 8].copy_from_slice(&7u64.to_le_bytes());
        b[0x368..0x370].copy_from_slice(&u64::MAX.to_le_bytes());
        for (i, e) in entries.iter().enumerate() {
            b[start + i * 4..start + i * 4 + 4].copy_from_slice(e);
        }
        if with_path {
            let path: Vec<u8> = r"\??\D:\Xbox\C4984363\Content".encode_utf16().flat_map(u16::to_le_bytes).collect();
            let tail = start + entries.len() * 4;
            b[tail..tail + path.len()].copy_from_slice(&path);
        }
        b
    }

    #[test]
    fn marks_every_chunk_resident_and_leaves_completion_markers() {
        let plan = parse_plan(&plan_bytes(&[0, XSP_COPY_FLAG, 0])).unwrap();
        assert_eq!(plan.build_id.to_string(), BUILD);
        let mut xvi = xvi_bytes(4362, &[[2, 0, 0, 1], [0; 4], [0xC8, 0, 0, 0]]);
        let before = mark_resident(&mut xvi, &plan, r"\??\E:\Other\Content", &[]).unwrap();
        assert_eq!(before.missing, 2, "copy entry wants state 3, in-flight C8 is not resident");
        assert_eq!(before.content_path.as_deref(), Some(r"\??\D:\Xbox\C4984363\Content"));
        assert_eq!(
            inspect_residency(&xvi, &plan).unwrap().content_path.as_deref(),
            Some(r"\??\D:\Xbox\C4984363\Content"),
            "a recorded path is the service's and is kept"
        );
        assert_eq!(inspect_residency(&xvi, &plan).unwrap().missing, 0);
        assert_eq!(u64_at(&xvi, XVI_STREAMED).unwrap(), 1000);
        assert_eq!(u64_at(&xvi, XVI_RESIDENT).unwrap(), 3000);
        assert_eq!(u64_at(&xvi, 0x368).unwrap(), u64::MAX, "finalization marker belongs to the service");
    }

    #[test]
    fn leaves_the_chunks_of_service_written_files_missing() {
        let plan = parse_plan(&plan_bytes(&[0, 0, 0, 0])).unwrap();
        assert_eq!(plan.pages[2], 20..30);
        // A file on pages 15..21 touches records 1 and 2; one on page 39 touches 3.
        let records = plan.records_covering(&[15..21, 39..40]);
        assert_eq!(records, vec![1, 2, 3]);
        assert!(plan.records_covering(&[10..20]) == vec![1], "ranges are half-open");
        let mut xvi = xvi_bytes(4362, &[[2, 0, 0, 1], [2, 0, 0, 1], [0x19, 2, 0, 0], [0; 4]]);
        mark_resident(&mut xvi, &plan, r"\??\D:\X\Content", &[2]).unwrap();
        let start = table_start(4362);
        assert_eq!(&xvi[start + 8..start + 12], &[0; 4], "an in-flight chunk is reset to missing");
        assert_eq!(&xvi[start + 12..start + 16], &[2, 0, 0, 1]);
        assert_eq!(inspect_residency(&xvi, &plan).unwrap().missing, 1);
        // The 10 pages left out exceed the fixture's totals of 1000 and 3000 bytes.
        assert_eq!(u64_at(&xvi, XVI_STREAMED).unwrap(), 0);
        assert_eq!(u64_at(&xvi, XVI_RESIDENT).unwrap(), 0);
    }

    #[test]
    fn finds_the_table_from_the_data_not_the_segment_count() {
        // NINJA GAIDEN 4 paused at 313 MB: 3768 segments, table at 0x8A2, one
        // byte past where the Pentiment/Ori/Resonance formula puts it.
        let plan = parse_plan(&plan_bytes(&[0, 0, 0, 0])).unwrap();
        let entries = [[2, 0, 0, 1], [2, 0, 0, 1], [0x0F, 0x3D, 1, 0], [0; 4]];
        assert_eq!(table_start(3768), 0x8A1);
        for with_path in [true, false] {
            let mut xvi = xvi_at(3768, 0x8A2, &entries, with_path);
            let found = inspect_residency(&xvi, &plan).unwrap();
            assert_eq!(found.missing, 2);
            assert_eq!(found.content_path.is_some(), with_path);
            mark_resident(&mut xvi, &plan, r"\??\D:\Xbox\8DD6F35D\Content", &[]).unwrap();
            assert_eq!(&xvi[0x8A2 + 8..0x8A2 + 12], &[2, 0, 0, 1]);
            assert_eq!(xvi[0x8A1], 0, "nothing written before the table");
        }
        // Small and large real offsets are inside the plausible window.
        for (segments, start) in [(87, 0x507), (10, 0x4F5), (14726, 0x4F2 + 3694), (180, 0x4F2 + 49)] {
            assert!(plausible_table_start(start, segments), "{segments} segments at 0x{start:X}");
        }
        assert!(!plausible_table_start(0x935, 100));
    }

    #[test]
    fn refuses_an_index_with_nothing_streamed() {
        let plan = parse_plan(&plan_bytes(&[0, 0])).unwrap();
        let xvi = fresh_xvi_bytes(4362, &[[0; 4], [0; 4]]);
        assert!(inspect_residency(&xvi, &plan).unwrap_err().contains("not streamed anything"));
    }

    #[test]
    fn accepts_in_flight_entries() {
        let plan = parse_plan(&plan_bytes(&[0, 0, 0])).unwrap();
        let xvi = fresh_xvi_bytes(45651, &[[2, 0, 0, 1], [0xBA, 0x19, 0, 0], [0; 4]]);
        assert_eq!(inspect_residency(&xvi, &plan).unwrap().missing, 2);
    }

    #[test]
    fn records_the_content_path_on_a_freshly_paused_install() {
        let plan = parse_plan(&plan_bytes(&[0, 0])).unwrap();
        let mut xvi = fresh_xvi_bytes(45651, &[[2, 0, 0, 1], [0; 4]]);
        let before = mark_resident(&mut xvi, &plan, r"\??\D:\Xbox\7B0ECD90\Content", &[]).unwrap();
        assert_eq!((before.missing, before.content_path), (1, None));
        let after = inspect_residency(&xvi, &plan).unwrap();
        assert_eq!((after.missing, after.content_path.as_deref()), (0, Some(r"\??\D:\Xbox\7B0ECD90\Content")));
    }

    #[test]
    fn refuses_a_table_it_does_not_understand() {
        let plan = parse_plan(&plan_bytes(&[0, 0])).unwrap();
        // Wrong segment count shifts the table, so the path check fails.
        let mut xvi = xvi_bytes(4362, &[[0; 4], [0; 4]]);
        xvi[XVI_SEGMENT_COUNT..XVI_SEGMENT_COUNT + 4].copy_from_slice(&100u32.to_le_bytes());
        assert!(mark_resident(&mut xvi.clone(), &plan, r"\??\D:\X\Content", &[]).is_err());
        let three = parse_plan(&plan_bytes(&[0, 0, 0])).unwrap();
        assert!(inspect_residency(&xvi_bytes(8, &[[0; 4], [0; 4]]), &three).is_err());
    }

    #[test]
    fn reads_segment_paths_and_sizes() {
        let names = ["a.exe", r"Data\b.pak"];
        let mut b = vec![0u8; 100];
        b[..4].copy_from_slice(SMD_MAGIC);
        b[12..16].copy_from_slice(&100u32.to_le_bytes());
        b[16..20].copy_from_slice(&2u32.to_le_bytes());
        let mut paths = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut seg = [0u8; 16];
            seg[2..4].copy_from_slice(&(units.len() as u16).to_le_bytes());
            seg[4..8].copy_from_slice(&((paths.len()) as u32).to_le_bytes());
            seg[8..16].copy_from_slice(&((i as u64 + 1) * 10).to_le_bytes());
            b.extend_from_slice(&seg);
            paths.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        }
        b.extend_from_slice(&paths);
        let segs = parse_segments(&b).unwrap();
        assert_eq!(segs, vec![
            Segment { path: "a.exe".into(), size: 10 },
            Segment { path: r"Data\b.pak".into(), size: 20 },
        ]);
    }
}
