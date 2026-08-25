//! Columns of byte rows: the Stride structural layout, and which layout
//! one of these columns takes.
//!
//! A byte row column is FullZip (`crate::fullzip`) unless every row of
//! it is the same length, in which case it is Stride. Stride is the
//! layout with nothing in it: `stride: u32`, then `value_count * stride`
//! bytes of rows end to end. There is no chunk index, because row `i`
//! begins at `4 + i * stride` and nothing has to be walked to find it,
//! and there is no compression pass, because the columns this layout is
//! for hold numbers rather than text. A 768 dimension `FLOAT32`
//! embedding is the case it exists for: FullZip trains a symbol table on
//! a sample of the column and then encodes every chunk twice to find out
//! that float bytes do not compress, which is a scan of the column for
//! an answer known before it starts.
//!
//! Which layout a column took is in its meta, so no caller has to
//! remember: [`write_rows`] chooses and [`read_rows`] and
//! [`read_rows_range`] ask. A column that stops being uniform, because a
//! row was written into it that is a different length, becomes FullZip
//! at the next write with nothing to migrate.
//!
//! The crc covers the whole payload and is verified by [`read_rows`],
//! the full scan. The point path skips it the way the other two layouts
//! do, and bounds every access against the meta instead: the one
//! structural claim a Stride payload makes is that `4 + count * stride`
//! is its length, and that is checked before any row is named.

use std::collections::BTreeMap;

use zu_common::{Result, ZuError};

use crate::file::Zu1File;
use crate::fullzip::{
    read_blob_range, read_blob_segment, rewrite_blob_reordered, rewrite_blob_segment,
    write_blob_segment,
};
use crate::segment::{SegmentMeta, Structural, read_payload_span};

fn corrupt(detail: &str) -> ZuError {
    ZuError::Corrupt {
        what: "stride segment",
        detail: detail.to_string(),
    }
}

/// The length every row of `values` has, or `None` for a column no
/// stride describes.
///
/// A column of no rows has no stride to speak of and a column of empty
/// rows would give a stride of zero, which is a divisor. Both go to
/// FullZip, where they cost nothing anyway.
pub fn uniform_len(values: &[&[u8]]) -> Option<usize> {
    let stride = values.first()?.len();
    match stride > 0 && values.iter().all(|v| v.len() == stride) {
        true => Some(stride),
        false => None,
    }
}

/// Writes a column of byte rows in whichever layout fits it.
pub fn write_rows(db: &mut Zu1File, values: &[&[u8]]) -> Result<SegmentMeta> {
    match uniform_len(values) {
        Some(stride) => write_stride_segment(db, values, stride),
        None => write_blob_segment(db, values),
    }
}

/// Writes `values` as a Stride payload. Every row must be `stride` long,
/// which is what [`uniform_len`] was asked.
fn write_stride_segment(db: &mut Zu1File, values: &[&[u8]], stride: usize) -> Result<SegmentMeta> {
    let mut payload = Vec::with_capacity(4 + values.len() * stride);
    payload.extend_from_slice(&(stride as u32).to_le_bytes());
    for v in values {
        if v.len() != stride {
            return Err(ZuError::InvalidArgument(format!(
                "a stride {stride} column does not hold a {} byte row",
                v.len()
            )));
        }
        payload.extend_from_slice(v);
    }
    let crc = crc32c::crc32c(&payload);
    let (blocks, start) = db.pack_bytes(&payload)?;
    Ok(SegmentMeta {
        value_count: values.len() as u64,
        payload_len: payload.len() as u64,
        uncompressed_bytes: (values.len() * stride) as u64,
        // The zone map does not apply to byte payloads, the way it does
        // not for FullZip.
        min: 0,
        max: 0,
        crc,
        structural: Structural::Stride,
        sorted: false,
        start,
        blocks,
    })
}

/// [`crate::fullzip::rewrite_blob_segment`] for a column that may be in
/// either layout: `updates` written over the rows they name and
/// `appended` added on the end.
///
/// A FullZip column keeps the per chunk rewrite it has, which is what
/// that function is for. A Stride one has no chunks to spare and no
/// encoding to skip, so it is read out and written again, which moves
/// the same bytes the FullZip path moves and does none of the work
/// around them. A column that stops being uniform, which is what a
/// null does, comes back as FullZip and nothing has to migrate it.
///
/// A FullZip column that becomes uniform stays FullZip. Deciding the
/// other way would mean a column changing layout under a single cell
/// write, and the layout is worth having where the column is written
/// whole, which is where the choice is made.
pub fn rewrite_rows(
    db: &mut Zu1File,
    old: &SegmentMeta,
    updates: &BTreeMap<u64, Vec<u8>>,
    appended: &[Vec<u8>],
) -> Result<SegmentMeta> {
    if old.structural != Structural::Stride {
        return rewrite_blob_segment(db, old, updates, appended);
    }
    let base = old.value_count;
    if let Some((&row, _)) = updates.iter().next_back()
        && row >= base
    {
        return Err(ZuError::InvalidArgument(format!(
            "update at row {row} past the {base} rows the segment holds"
        )));
    }
    let (mut bytes, mut ends) = (Vec::new(), Vec::new());
    read_rows(db, old, &mut bytes, &mut ends)?;
    let mut values = spans(&bytes, &ends);
    for (&row, value) in updates {
        values[row as usize] = &value[..];
    }
    values.extend(appended.iter().map(|v| &v[..]));
    write_rows(db, &values)
}

/// [`crate::fullzip::rewrite_blob_reordered`] for a column that may be
/// in either layout: the same values again in a new order.
///
/// The FullZip path exists to keep the symbol table the column was
/// encoded with rather than train a second one on the same bytes. A
/// Stride column has no table, so a reorder is the write and nothing
/// else.
pub fn rewrite_rows_reordered(
    db: &mut Zu1File,
    old: &SegmentMeta,
    values: &[&[u8]],
) -> Result<SegmentMeta> {
    match (uniform_len(values), old.structural) {
        (Some(stride), _) => write_stride_segment(db, values, stride),
        (None, Structural::Stride) => write_blob_segment(db, values),
        (None, _) => rewrite_blob_reordered(db, old, values),
    }
}

/// The rows a `bytes`, `ends` pair holds, as the slices every writer
/// here takes.
pub fn spans<'a>(bytes: &'a [u8], ends: &[u64]) -> Vec<&'a [u8]> {
    let mut out = Vec::with_capacity(ends.len());
    let mut start = 0usize;
    for &end in ends {
        out.push(&bytes[start..end as usize]);
        start = end as usize;
    }
    out
}

/// The stride a Stride payload claims, checked against the meta.
///
/// This is the whole of what a Stride payload has to be believed about,
/// so it is asked once at the top of every read and every offset below
/// it follows from the answer.
fn stride_of(db: &mut Zu1File, meta: &SegmentMeta) -> Result<usize> {
    let head = read_payload_span(db, meta, 0, 4)?;
    let stride = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
    if stride == 0 {
        return Err(corrupt("stride of zero"));
    }
    let want = 4u64
        .checked_add(meta.value_count.saturating_mul(stride as u64))
        .ok_or_else(|| corrupt("stride times rows overflows"))?;
    if want != meta.payload_len || meta.uncompressed_bytes != meta.payload_len - 4 {
        return Err(corrupt("stride disagrees with the meta"));
    }
    Ok(stride)
}

/// Reads a byte row column back whole, verifying the payload crc, and
/// appends every row to `bytes_out` with its end offset in `ends_out`.
pub fn read_rows(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    bytes_out: &mut Vec<u8>,
    ends_out: &mut Vec<u64>,
) -> Result<()> {
    if meta.structural != Structural::Stride {
        return read_blob_segment(db, meta, bytes_out, ends_out);
    }
    let stride = stride_of(db, meta)?;
    // The span reader is the one that does not care which layout it is
    // reading, so the whole payload comes off it and the crc is checked
    // here. That is the only thing the full scan does that the point
    // path does not, because a Stride payload has no index to verify:
    // `stride_of` has already checked the one claim it makes.
    let payload = read_payload_span(db, meta, 0, meta.payload_len as usize)?;
    if crc32c::crc32c(&payload) != meta.crc {
        return Err(corrupt("payload crc mismatch"));
    }
    let base = bytes_out.len();
    bytes_out.extend_from_slice(&payload[4..]);
    for i in 0..meta.value_count as usize {
        ends_out.push((base + (i + 1) * stride) as u64);
    }
    Ok(())
}

/// Point access: appends rows `[start, end)` to `bytes_out` and their
/// end offsets to `ends_out`, reading only the bytes they occupy.
pub fn read_rows_range(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    start: u64,
    end: u64,
    bytes_out: &mut Vec<u8>,
    ends_out: &mut Vec<u64>,
) -> Result<()> {
    if meta.structural != Structural::Stride {
        return read_blob_range(db, meta, start, end, bytes_out, ends_out);
    }
    if start > end || end > meta.value_count {
        return Err(ZuError::InvalidArgument(format!(
            "range {start}..{end} out of 0..{}",
            meta.value_count
        )));
    }
    if start == end {
        return Ok(());
    }
    let stride = stride_of(db, meta)?;
    let from = 4 + start as usize * stride;
    let to = 4 + end as usize * stride;
    let span = read_payload_span(db, meta, from, to)?;
    let base = bytes_out.len();
    bytes_out.extend_from_slice(&span);
    for i in 0..(end - start) as usize {
        ends_out.push((base + (i + 1) * stride) as u64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &std::path::Path) -> Zu1File {
        Zu1File::create(&dir.join("rows.zu1")).unwrap()
    }

    #[test]
    fn a_column_of_equal_rows_takes_the_stride_layout() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = file(dir.path());
        let rows: Vec<Vec<u8>> = (0..2500u32)
            .map(|i| i.to_le_bytes().repeat(4).to_vec())
            .collect();
        let refs: Vec<&[u8]> = rows.iter().map(|r| &r[..]).collect();
        let meta = write_rows(&mut db, &refs).unwrap();
        assert_eq!(meta.structural, Structural::Stride);
        // Nothing but the rows and the stride word, which is the point
        // of the layout: no per row length, no chunk index, no table.
        assert_eq!(meta.payload_len, 4 + 2500 * 16);

        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_rows(&mut db, &meta, &mut bytes, &mut ends).unwrap();
        assert_eq!(ends.len(), 2500);
        for (i, row) in rows.iter().enumerate() {
            let lo = if i == 0 { 0 } else { ends[i - 1] as usize };
            assert_eq!(&bytes[lo..ends[i] as usize], &row[..], "row {i}");
        }
        // And a range read of the middle gives the same rows without
        // reading the ones around them.
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_rows_range(&mut db, &meta, 1000, 1003, &mut bytes, &mut ends).unwrap();
        assert_eq!(ends, vec![16, 32, 48]);
        assert_eq!(&bytes[..16], &rows[1000][..]);
        assert_eq!(&bytes[32..], &rows[1002][..]);
    }

    /// A column whose rows differ is FullZip, and one that is empty or
    /// all empty rows is FullZip too, because neither has a stride.
    #[test]
    fn a_column_no_stride_describes_stays_fullzip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = file(dir.path());
        for values in [
            vec![&b"aaaa"[..], &b"bbb"[..]],
            vec![],
            vec![&b""[..], &b""[..]],
        ] {
            let meta = write_rows(&mut db, &values).unwrap();
            assert_eq!(meta.structural, Structural::FullZip, "{values:?}");
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            read_rows(&mut db, &meta, &mut bytes, &mut ends).unwrap();
            assert_eq!(ends.len(), values.len());
        }
    }

    /// The one structural claim a Stride payload makes is its stride,
    /// and a meta that disagrees with it is refused before any row is
    /// named rather than after one is read out of the wrong place.
    #[test]
    fn a_stride_that_disagrees_with_the_meta_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = file(dir.path());
        let rows = [&b"abcd"[..], &b"efgh"[..], &b"ijkl"[..]];
        let meta = write_rows(&mut db, &rows).unwrap();
        for wrong in [
            SegmentMeta {
                value_count: 4,
                ..meta.clone()
            },
            SegmentMeta {
                uncompressed_bytes: 11,
                ..meta.clone()
            },
        ] {
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            assert!(read_rows(&mut db, &wrong, &mut bytes, &mut ends).is_err());
            assert!(
                read_rows_range(&mut db, &wrong, 0, 1, &mut bytes, &mut ends).is_err(),
                "{wrong:?}"
            );
        }
    }

    /// Both readers take either layout, so a caller holds a meta and
    /// not a decision.
    #[test]
    fn either_reader_takes_either_layout() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = file(dir.path());
        let same = [&b"abcd"[..], &b"efgh"[..]];
        let mixed = [&b"abcd"[..], &b"ef"[..]];
        for values in [&same[..], &mixed[..]] {
            let meta = write_rows(&mut db, values).unwrap();
            let (mut whole, mut whole_ends) = (Vec::new(), Vec::new());
            read_rows(&mut db, &meta, &mut whole, &mut whole_ends).unwrap();
            let (mut part, mut part_ends) = (Vec::new(), Vec::new());
            read_rows_range(&mut db, &meta, 0, 2, &mut part, &mut part_ends).unwrap();
            assert_eq!((whole, whole_ends), (part, part_ends));
        }
    }
}
