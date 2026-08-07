//! Differential fuzz of the FullZip writer against its readers: the
//! input parses into u16-length-prefixed rows, the writer stores them in
//! a real file, and the full scan and the point path must both hand
//! every row back byte for byte. This is the property the format
//! promises; any divergence, panic, or error on writer output is a
//! finding. Hostile bytes against the readers alone live in
//! `zu1_decode` and `zu1_file`.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use zu_zu1::file::Zu1File;
use zu_zu1::fullzip;

fn scratch_path() -> &'static PathBuf {
    static DIR: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, path) = DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("fuzz.zu1");
        (dir, path)
    });
    path
}

fn value<'a>(bytes: &'a [u8], ends: &[u64], i: usize) -> &'a [u8] {
    let lo = if i == 0 { 0 } else { ends[i - 1] as usize };
    &bytes[lo..ends[i] as usize]
}

fuzz_target!(|data: &[u8]| {
    let mut rows: Vec<&[u8]> = Vec::new();
    let mut rest = data;
    while let Some((len_bytes, tail)) = rest.split_at_checked(2) {
        // 1024 rows of 16380 bytes zip to exactly the 16 MiB chunk raw
        // cap, so no parsed input can make the writer refuse.
        let want = (u16::from_le_bytes(len_bytes.try_into().unwrap()) as usize).min(16380);
        let (row, tail) = tail.split_at(want.min(tail.len()));
        rows.push(row);
        rest = tail;
        if rows.len() >= 4096 {
            break;
        }
    }
    let path = scratch_path();
    std::fs::remove_file(path).ok();
    let mut db = Zu1File::create(path).expect("create");
    let meta = fullzip::write_blob_segment(&mut db, &rows).expect("write valid rows");
    let (mut bytes, mut ends) = (Vec::new(), Vec::new());
    fullzip::read_blob_segment(&mut db, &meta, &mut bytes, &mut ends).expect("full scan");
    assert_eq!(ends.len(), rows.len());
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(value(&bytes, &ends, i), *row, "full scan row {i}");
    }
    if rows.is_empty() {
        return;
    }
    // One range and one row through the point path, both driven by the
    // input so the fuzzer explores chunk boundaries.
    let n = rows.len() as u64;
    let a = u64::from(*data.first().unwrap_or(&0));
    let b = u64::from(*data.get(1).unwrap_or(&0));
    let start = (a * 17) % n;
    let end = start + 1 + (b * 31) % (n - start);
    let (mut pb, mut pe) = (Vec::new(), Vec::new());
    fullzip::read_blob_range(&mut db, &meta, start, end, &mut pb, &mut pe).expect("range");
    assert_eq!(pe.len(), (end - start) as usize);
    for (k, i) in (start as usize..end as usize).enumerate() {
        assert_eq!(value(&pb, &pe, k), rows[i], "range row {i}");
    }
    let mut one = Vec::new();
    fullzip::read_blob_row(&mut db, &meta, start, &mut one).expect("row");
    assert_eq!(one, rows[start as usize], "row {start}");
});
