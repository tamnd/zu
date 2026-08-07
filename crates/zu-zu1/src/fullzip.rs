//! Column segments in the FullZip structural layout of
//! `docs/04-storage-zu1-format.md` §3: variable-width values zipped with
//! their lengths, for STRING past the dict threshold, BLOB, VECTOR, and
//! LIST payloads. Zipping means a row range is one contiguous byte range,
//! and the per-chunk offset samples make the seek to any row O(1): jump
//! to the chunk holding it, then walk at most 1023 inline lengths.
//!
//! Payload layout, all little-endian: `chunk_count: u32`, then one `u64`
//! cumulative compressed end offset per chunk (relative to the body),
//! then one `u64` cumulative zipped size per chunk, then the body of
//! concatenated chunks. Each chunk covers up to 1024 rows and starts with
//! one encoding id byte: Plain (0) is the zipped form itself, one
//! `len: u32` before each row's bytes, and FSST (8) is that form behind
//! `zu_encoding::fsst`, self-contained symbol table and all. The writer
//! keeps whichever is smaller per chunk. The zipped sizes are what let a
//! reader size and verify every chunk before touching it: the FSST
//! ceiling is exact, a plain chunk must match to the byte, and a chunk
//! claiming more than [`MAX_CHUNK_RAW`] is rejected before anything is
//! allocated.
//!
//! The meta's `uncompressed_bytes` holds the total value bytes (lengths
//! excluded), and the full scan cross-checks the decoded total against
//! it. The zone map does not apply to byte payloads, so FullZip metas
//! carry `min = max = 0`. As with MiniBlock, the full scan verifies the
//! payload crc and every structural claim; the point path skips the crc
//! by design and bounds every access instead.

use zu_common::{Result, ZuError};
use zu_encoding::{EncodingId, fsst};

use crate::BLOCK_SIZE;
use crate::file::Zu1File;
use crate::segment::{CHUNK_ROWS, SegmentMeta, Structural, read_payload_span};

/// Format rule: a chunk's zipped form holds at most 16 MiB. Chunks cover
/// at most 1024 rows, so this admits values averaging 16 KiB per chunk
/// while bounding what a hostile index can make a reader allocate.
/// Larger single values need the continuation design that arrives with
/// the column catalog; the writer refuses them rather than truncate.
pub const MAX_CHUNK_RAW: usize = 1 << 24;

fn corrupt(detail: &str) -> ZuError {
    ZuError::Corrupt {
        what: "fullzip segment",
        detail: detail.to_string(),
    }
}

/// Encodes `values` into the FullZip payload, appending to `out`, and
/// returns the total value bytes. Public so the fuzz seeds can build a
/// payload without a file around it.
pub fn encode_payload(values: &[&[u8]], out: &mut Vec<u8>) -> Result<u64> {
    let chunk_count = values.len().div_ceil(CHUNK_ROWS);
    let mut body = Vec::new();
    let mut comp_ends = Vec::with_capacity(chunk_count);
    let mut raw_ends = Vec::with_capacity(chunk_count);
    let mut zipped = Vec::new();
    let mut packed = Vec::new();
    let mut raw_total = 0u64;
    let mut value_bytes = 0u64;
    for chunk in values.chunks(CHUNK_ROWS) {
        zipped.clear();
        for v in chunk {
            if zipped.len() + 4 + v.len() > MAX_CHUNK_RAW {
                return Err(ZuError::InvalidArgument(format!(
                    "fullzip chunk exceeds the {MAX_CHUNK_RAW} byte raw cap"
                )));
            }
            zipped.extend_from_slice(&(v.len() as u32).to_le_bytes());
            zipped.extend_from_slice(v);
            value_bytes += v.len() as u64;
        }
        packed.clear();
        let packed_len = fsst::encode(&zipped, &mut packed);
        if packed_len < zipped.len() {
            body.push(EncodingId::Fsst as u8);
            body.extend_from_slice(&packed);
        } else {
            body.push(EncodingId::Plain as u8);
            body.extend_from_slice(&zipped);
        }
        raw_total += zipped.len() as u64;
        comp_ends.push(body.len() as u64);
        raw_ends.push(raw_total);
    }
    out.reserve(4 + chunk_count * 16 + body.len());
    out.extend_from_slice(&(chunk_count as u32).to_le_bytes());
    for e in &comp_ends {
        out.extend_from_slice(&e.to_le_bytes());
    }
    for e in &raw_ends {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out.extend_from_slice(&body);
    Ok(value_bytes)
}

/// Encodes `values` chunk by chunk and writes the FullZip payload across
/// freshly allocated blocks.
pub fn write_blob_segment(db: &mut Zu1File, values: &[&[u8]]) -> Result<SegmentMeta> {
    let mut payload = Vec::new();
    let value_bytes = encode_payload(values, &mut payload)?;
    let crc = crc32c::crc32c(&payload);
    let mut blocks = Vec::new();
    let mut block = vec![0u8; BLOCK_SIZE as usize];
    for part in payload.chunks(BLOCK_SIZE as usize) {
        let ptr = db.allocate_block();
        block[..part.len()].copy_from_slice(part);
        block[part.len()..].fill(0);
        db.write_block(ptr, &block)?;
        blocks.push(ptr);
    }
    Ok(SegmentMeta {
        value_count: values.len() as u64,
        payload_len: payload.len() as u64,
        uncompressed_bytes: value_bytes,
        min: 0,
        max: 0,
        crc,
        structural: Structural::FullZip,
        blocks,
    })
}

/// One chunk's zipped bytes: borrowed straight from a plain chunk,
/// FSST-decoded into `scratch` otherwise. `raw_size` comes from the
/// index and every path checks it against [`MAX_CHUNK_RAW`] first, so it
/// is both the exact decode ceiling and the verified result size.
fn unpack_chunk<'a>(
    chunk: &'a [u8],
    raw_size: usize,
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    let (&enc, payload) = chunk
        .split_first()
        .ok_or_else(|| corrupt("empty chunk"))?;
    match enc {
        e if e == EncodingId::Plain as u8 => {
            if payload.len() != raw_size {
                return Err(corrupt("plain chunk disagrees with its zipped size"));
            }
            Ok(payload)
        }
        e if e == EncodingId::Fsst as u8 => {
            scratch.clear();
            fsst::decode(payload, raw_size, scratch)?;
            if scratch.len() != raw_size {
                return Err(corrupt("fsst chunk disagrees with its zipped size"));
            }
            Ok(scratch)
        }
        id => Err(ZuError::Unsupported {
            what: "fullzip chunk encoding",
            id: u32::from(id),
        }),
    }
}

/// Appends rows `[lo, hi)` of one zipped chunk to `bytes_out` and their
/// absolute end offsets to `ends_out`, walking the inline lengths from
/// the chunk start. With `strict`, the walk must consume the chunk
/// exactly through row `total`, which is what the full scan holds every
/// chunk to.
fn unzip_rows(
    zipped: &[u8],
    total: usize,
    lo: usize,
    hi: usize,
    strict: bool,
    bytes_out: &mut Vec<u8>,
    ends_out: &mut Vec<u64>,
) -> Result<()> {
    let mut pos = 0usize;
    let last = if strict { total } else { hi };
    for row in 0..last {
        let len = u32::from_le_bytes(
            zipped
                .get(pos..pos + 4)
                .ok_or_else(|| corrupt("truncated row length"))?
                .try_into()
                .unwrap(),
        ) as usize;
        pos += 4;
        let bytes = zipped
            .get(pos..pos + len)
            .ok_or_else(|| corrupt("truncated row bytes"))?;
        pos += len;
        if row >= lo && row < hi {
            bytes_out.extend_from_slice(bytes);
            ends_out.push(bytes_out.len() as u64);
        }
    }
    if strict && pos != zipped.len() {
        return Err(corrupt("trailing bytes after the last row"));
    }
    Ok(())
}

/// Decodes a full FullZip payload, appending every value's bytes to
/// `bytes_out` and each value's absolute end offset in `bytes_out` to
/// `ends_out`, so value `i` of an initially empty pair is
/// `bytes_out[ends_out[i - 1]..ends_out[i]]` with `ends_out[-1]` read as
/// zero. Verifies every chunk against the index, every row against its
/// chunk, and the value byte total against `uncompressed_bytes`. A
/// rejected payload leaves both vectors untouched. Public alongside the
/// other container decoders so the fuzz targets reach it without a file.
pub fn decode_payload(
    payload: &[u8],
    value_count: u64,
    uncompressed_bytes: u64,
    bytes_out: &mut Vec<u8>,
    ends_out: &mut Vec<u64>,
) -> Result<()> {
    let base_bytes = bytes_out.len();
    let base_ends = ends_out.len();
    let run = |bytes_out: &mut Vec<u8>, ends_out: &mut Vec<u64>| -> Result<()> {
        let chunks = value_count.div_ceil(CHUNK_ROWS as u64) as usize;
        let head = payload
            .get(..4)
            .ok_or_else(|| corrupt("truncated index"))?;
        if u32::from_le_bytes(head.try_into().unwrap()) as usize != chunks {
            return Err(corrupt("chunk count disagrees with meta"));
        }
        let index = payload
            .get(4..4 + chunks * 16)
            .ok_or_else(|| corrupt("truncated index"))?;
        let body = &payload[4 + chunks * 16..];
        let at = |i: usize| u64::from_le_bytes(index[i * 8..i * 8 + 8].try_into().unwrap());
        let mut prev_comp = 0usize;
        let mut prev_raw = 0u64;
        let mut scratch = Vec::new();
        for i in 0..chunks {
            let comp_end = at(i) as usize;
            let raw_end = at(chunks + i);
            if comp_end < prev_comp || comp_end > body.len() {
                return Err(corrupt("chunk index not monotone"));
            }
            let raw_size = raw_end
                .checked_sub(prev_raw)
                .ok_or_else(|| corrupt("chunk index not monotone"))?;
            if raw_size > MAX_CHUNK_RAW as u64 {
                return Err(corrupt("chunk above the raw cap"));
            }
            let rows = (value_count as usize - i * CHUNK_ROWS).min(CHUNK_ROWS);
            let zipped = unpack_chunk(&body[prev_comp..comp_end], raw_size as usize, &mut scratch)?;
            unzip_rows(zipped, rows, 0, rows, true, bytes_out, ends_out)?;
            prev_comp = comp_end;
            prev_raw = raw_end;
        }
        if prev_comp != body.len() {
            return Err(corrupt("trailing bytes after the last chunk"));
        }
        if (bytes_out.len() - base_bytes) as u64 != uncompressed_bytes {
            return Err(corrupt("value bytes disagree with meta"));
        }
        Ok(())
    };
    run(bytes_out, ends_out).inspect_err(|_| {
        bytes_out.truncate(base_bytes);
        ends_out.truncate(base_ends);
    })
}

/// Reads a segment back, verifying the payload crc, and appends every
/// value to `bytes_out` and `ends_out` as [`decode_payload`] does.
pub fn read_blob_segment(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    bytes_out: &mut Vec<u8>,
    ends_out: &mut Vec<u64>,
) -> Result<()> {
    if meta.structural != Structural::FullZip {
        return Err(corrupt("FullZip reader given a MiniBlock segment"));
    }
    // The claimed length only seeds the reservation; growth past the cap
    // is bounded by the block reads, which fail on the first bad pointer.
    let mut payload = Vec::with_capacity((meta.payload_len as usize).min(1 << 22));
    for &ptr in &meta.blocks {
        let block = db.read_block(ptr)?;
        let want = (meta.payload_len as usize - payload.len()).min(block.len());
        payload.extend_from_slice(&block[..want]);
    }
    if payload.len() != meta.payload_len as usize {
        return Err(corrupt("payload shorter than meta claims"));
    }
    if crc32c::crc32c(&payload) != meta.crc {
        return Err(corrupt("payload crc mismatch"));
    }
    decode_payload(
        &payload,
        meta.value_count,
        meta.uncompressed_bytes,
        bytes_out,
        ends_out,
    )
}

/// Point access: appends values `[start, end)` to `bytes_out` and their
/// absolute end offsets to `ends_out`, decoding only the chunks that
/// cover the range and reading only the bytes they occupy.
pub fn read_blob_range(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    start: u64,
    end: u64,
    bytes_out: &mut Vec<u8>,
    ends_out: &mut Vec<u64>,
) -> Result<()> {
    if start > end || end > meta.value_count {
        return Err(ZuError::InvalidArgument(format!(
            "range {start}..{end} out of 0..{}",
            meta.value_count
        )));
    }
    if meta.structural != Structural::FullZip {
        return Err(corrupt("FullZip reader given a MiniBlock segment"));
    }
    if start == end {
        return Ok(());
    }
    let base_bytes = bytes_out.len();
    let base_ends = ends_out.len();
    let run = |db: &mut Zu1File, bytes_out: &mut Vec<u8>, ends_out: &mut Vec<u64>| -> Result<()> {
        let chunks = meta.chunk_count();
        let body_off = 4 + chunks * 16;
        let first = start as usize / CHUNK_ROWS;
        let last = (end - 1) as usize / CHUNK_ROWS;
        // The index span needs the end of chunk `first - 1` to know where
        // chunk `first` starts; chunk 0 starts at the body.
        let lo = first.saturating_sub(1);
        let comp_span = read_payload_span(db, meta, 4 + lo * 8, 4 + (last + 1) * 8)?;
        let raw_span = read_payload_span(
            db,
            meta,
            4 + (chunks + lo) * 8,
            4 + (chunks + last + 1) * 8,
        )?;
        let ent = |span: &[u8], chunk: usize| {
            let i = chunk - lo;
            u64::from_le_bytes(span[i * 8..i * 8 + 8].try_into().unwrap())
        };
        let body_start = if first == 0 {
            0
        } else {
            ent(&comp_span, first - 1) as usize
        };
        let body_end = ent(&comp_span, last) as usize;
        if body_start > body_end || body_off + body_end > meta.payload_len as usize {
            return Err(corrupt("chunk index not monotone"));
        }
        let bytes = read_payload_span(db, meta, body_off + body_start, body_off + body_end)?;
        let mut scratch = Vec::new();
        let mut prev_comp = body_start;
        for i in first..=last {
            let comp_end = ent(&comp_span, i) as usize;
            if comp_end < prev_comp {
                return Err(corrupt("chunk index not monotone"));
            }
            let prev_raw = if i == 0 { 0 } else { ent(&raw_span, i - 1) };
            let raw_size = ent(&raw_span, i)
                .checked_sub(prev_raw)
                .ok_or_else(|| corrupt("chunk index not monotone"))?;
            if raw_size > MAX_CHUNK_RAW as u64 {
                return Err(corrupt("chunk above the raw cap"));
            }
            let rows = meta.chunk_rows(i);
            let lo_row = (start as usize).max(i * CHUNK_ROWS) - i * CHUNK_ROWS;
            let hi_row = (end as usize).min((i + 1) * CHUNK_ROWS) - i * CHUNK_ROWS;
            let zipped = unpack_chunk(
                &bytes[prev_comp - body_start..comp_end - body_start],
                raw_size as usize,
                &mut scratch,
            )?;
            unzip_rows(zipped, rows, lo_row, hi_row, false, bytes_out, ends_out)?;
            prev_comp = comp_end;
        }
        Ok(())
    };
    run(db, bytes_out, ends_out).inspect_err(|_| {
        bytes_out.truncate(base_bytes);
        ends_out.truncate(base_ends);
    })
}

/// Point access to one row: appends value `row`'s bytes to `out`.
pub fn read_blob_row(db: &mut Zu1File, meta: &SegmentMeta, row: u64, out: &mut Vec<u8>) -> Result<()> {
    let end = row
        .checked_add(1)
        .ok_or_else(|| ZuError::InvalidArgument(format!("row {row} out of range")))?;
    let mut ends = Vec::with_capacity(1);
    read_blob_range(db, meta, row, end, out, &mut ends)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::read_segment;

    fn urls(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                format!(
                    "https://example.com/user/{}/posts?page={}",
                    i * 37,
                    i % 12
                )
                .into_bytes()
            })
            .collect()
    }

    fn refs(values: &[Vec<u8>]) -> Vec<&[u8]> {
        values.iter().map(|v| v.as_slice()).collect()
    }

    fn value<'a>(bytes: &'a [u8], ends: &[u64], i: usize) -> &'a [u8] {
        let lo = if i == 0 { 0 } else { ends[i - 1] as usize };
        &bytes[lo..ends[i] as usize]
    }

    #[test]
    fn roundtrip_compresses_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let values = urls(10_000);
        let meta = write_blob_segment(&mut db, &refs(&values)).unwrap();
        let raw: u64 = values.iter().map(|v| v.len() as u64).sum();
        assert_eq!(meta.uncompressed_bytes, raw);
        assert_eq!(meta.structural, Structural::FullZip);
        assert!(
            meta.payload_len * 2 < raw,
            "repetitive urls should halve, got {} from {raw}",
            meta.payload_len
        );
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_blob_segment(&mut db, &meta, &mut bytes, &mut ends).unwrap();
        assert_eq!(ends.len(), values.len());
        for (i, v) in values.iter().enumerate() {
            assert_eq!(value(&bytes, &ends, i), v.as_slice(), "row {i}");
        }
    }

    #[test]
    fn roundtrip_mixed_and_incompressible() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        // Random bytes defeat FSST so chunks fall back to plain, empty
        // values and a value far larger than a chunk's row slots mix in.
        let mut rng = 0xDEADBEEFu64;
        let mut values: Vec<Vec<u8>> = (0..3000usize)
            .map(|i| {
                (0..i % 97)
                    .map(|_| {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng as u8
                    })
                    .collect()
            })
            .collect();
        values[0] = Vec::new();
        values[1500] = vec![0xAB; 100_000];
        let meta = write_blob_segment(&mut db, &refs(&values)).unwrap();
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_blob_segment(&mut db, &meta, &mut bytes, &mut ends).unwrap();
        for (i, v) in values.iter().enumerate() {
            assert_eq!(value(&bytes, &ends, i), v.as_slice(), "row {i}");
        }
    }

    #[test]
    fn point_reads_match_full_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let values = urls(5000);
        let meta = write_blob_segment(&mut db, &refs(&values)).unwrap();
        let ranges = [
            (0u64, 1u64),
            (0, values.len() as u64),
            (1023, 1025),
            (1024, 2048),
            (2047, 2049),
            (3000, 3000),
            (4999, 5000),
            (500, 3777),
        ];
        for (s, e) in ranges {
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            read_blob_range(&mut db, &meta, s, e, &mut bytes, &mut ends).unwrap();
            assert_eq!(ends.len(), (e - s) as usize, "range {s}..{e}");
            for (k, i) in (s as usize..e as usize).enumerate() {
                assert_eq!(value(&bytes, &ends, k), values[i].as_slice(), "row {i}");
            }
        }
        let mut out = Vec::new();
        read_blob_row(&mut db, &meta, 4321, &mut out).unwrap();
        assert_eq!(out, values[4321]);
        let mut ends = Vec::new();
        assert!(read_blob_range(&mut db, &meta, 5, 4, &mut out, &mut ends).is_err());
        assert!(
            read_blob_range(&mut db, &meta, 0, 5001, &mut out, &mut ends).is_err()
        );
    }

    #[test]
    fn empty_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let meta = write_blob_segment(&mut db, &[]).unwrap();
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_blob_segment(&mut db, &meta, &mut bytes, &mut ends).unwrap();
        assert!(bytes.is_empty() && ends.is_empty());
    }

    #[test]
    fn oversized_value_refused_by_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let big = vec![0u8; MAX_CHUNK_RAW];
        assert!(write_blob_segment(&mut db, &[&big]).is_err());
    }

    #[test]
    fn corrupt_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let values = urls(3000);
        let meta = write_blob_segment(&mut db, &refs(&values)).unwrap();
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[100] ^= 0xFF;
        db.write_block(meta.blocks[0], &block).unwrap();
        let (mut bytes, mut ends) = (vec![7u8], vec![1u64]);
        assert!(read_blob_segment(&mut db, &meta, &mut bytes, &mut ends).is_err());
        assert_eq!(bytes, [7], "a rejected payload must not touch the outputs");
        assert_eq!(ends, [1]);
    }

    #[test]
    fn structural_mismatch_rejected_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let values = urls(100);
        let meta = write_blob_segment(&mut db, &refs(&values)).unwrap();
        let mut out = Vec::new();
        assert!(read_segment(&mut db, &meta, &mut out).is_err());
        let mut fake = meta.clone();
        fake.structural = Structural::MiniBlock;
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        assert!(read_blob_segment(&mut db, &fake, &mut bytes, &mut ends).is_err());
        assert!(read_blob_range(&mut db, &fake, 0, 1, &mut bytes, &mut ends).is_err());
    }

    #[test]
    fn hostile_index_cannot_flood_or_panic() {
        // A payload claiming a chunk far above the raw cap dies on the
        // cap check before the decoder allocates anything.
        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&2u64.to_le_bytes());
        payload.extend_from_slice(&(u64::from(u32::MAX) * 8).to_le_bytes());
        payload.push(EncodingId::Plain as u8);
        payload.push(0);
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        let err = decode_payload(&payload, 1, 10, &mut bytes, &mut ends).unwrap_err();
        assert!(format!("{err}").contains("raw cap"));
        // A raw total that wraps backwards is not monotone. Chunk 0 is a
        // legitimate chunk of 1024 empty values so the walk reaches the
        // wrapped entry, and the rejection must roll its rows back.
        let mut payload = 2u32.to_le_bytes().to_vec();
        for comp in [4097u64, 4098] {
            payload.extend_from_slice(&comp.to_le_bytes());
        }
        for raw in [4096u64, 10] {
            payload.extend_from_slice(&raw.to_le_bytes());
        }
        payload.push(EncodingId::Plain as u8);
        payload.extend_from_slice(&[0u8; 4096]);
        payload.push(EncodingId::Plain as u8);
        let err = decode_payload(&payload, 1025, 0, &mut bytes, &mut ends).unwrap_err();
        assert!(format!("{err}").contains("monotone"));
        assert!(bytes.is_empty() && ends.is_empty());
    }

    #[test]
    fn unknown_chunk_encoding_rejected_by_name() {
        let values: Vec<&[u8]> = vec![b"ab", b"cd"];
        let mut payload = Vec::new();
        encode_payload(&values, &mut payload).unwrap();
        // The chunk's encoding byte sits right after the index.
        let body = 4 + 16;
        payload[body] = 200;
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        let err = decode_payload(&payload, 2, 4, &mut bytes, &mut ends).unwrap_err();
        assert!(format!("{err}").contains("200"));
    }
}
