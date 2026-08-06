//! Column segments in the MiniBlock structural layout of
//! `docs/04-storage-zu1-format.md` §3: values packed in chunks of 1024,
//! each chunk an independently decodable `encode_auto` cascade, behind a
//! chunk index of u32 end offsets. A point read decodes only the chunks
//! covering the wanted rows and touches bytes on the order of the chunk,
//! not the segment.
//!
//! Payload layout, all little-endian: `chunk_count: u32`, then one `u32`
//! cumulative end offset per chunk (relative to the body), then one `u64`
//! fence per chunk holding the chunk's last value, then the body of
//! concatenated chunk cascades. The encoding id travels inside each
//! chunk, so the reader needs no side channel beyond the `SegmentMeta`.
//! The fences are what make [`probe`] cheap: inside any sorted row range
//! the fence of a fully covered chunk is an in-order sample, so a binary
//! search over fences names the one chunk that could hold a value and the
//! probe decodes that chunk alone. They cost 8 bytes per 1024 values,
//! about 0.06 bits per value.
//! Metas serialize into meta-block chains with a fixed layout:
//! `value_count: u64`, `payload_len: u64`, `uncompressed_bytes: u64`,
//! `crc32c: u32`, `block_count: u32`, then one `u64` per block pointer.
//!
//! The segment crc covers the whole payload and is verified by the full
//! scan path and `zu verify`. The point path skips it by design (checking
//! it would mean reading everything); it bounds every access by the meta
//! and rejects non-monotone indexes, truncated chunks, and count
//! mismatches instead.

use zu_common::{Result, ZuError};
use zu_encoding::segment as enc;

use crate::BLOCK_SIZE;
use crate::file::{BlockPtr, Zu1File};

/// Rows per MiniBlock chunk, the unit of point access.
pub const CHUNK_ROWS: usize = 1024;

/// Location and integrity data for one stored segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentMeta {
    pub value_count: u64,
    pub payload_len: u64,
    pub uncompressed_bytes: u64,
    pub crc: u32,
    pub blocks: Vec<BlockPtr>,
}

impl SegmentMeta {
    /// Serialized size in bytes.
    pub fn encoded_len(&self) -> usize {
        32 + self.blocks.len() * 8
    }

    /// Appends the meta to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.value_count.to_le_bytes());
        out.extend_from_slice(&self.payload_len.to_le_bytes());
        out.extend_from_slice(&self.uncompressed_bytes.to_le_bytes());
        out.extend_from_slice(&self.crc.to_le_bytes());
        out.extend_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        for b in &self.blocks {
            out.extend_from_slice(&b.to_le_bytes());
        }
    }

    /// Reads a meta from `bytes` starting at `pos`, returning the meta and
    /// the position after it.
    pub fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "segment meta",
            detail: detail.to_string(),
        };
        let head = bytes
            .get(pos..pos + 32)
            .ok_or_else(|| corrupt("truncated header"))?;
        let word = |i: usize| u64::from_le_bytes(head[i..i + 8].try_into().unwrap());
        let value_count = word(0);
        let payload_len = word(8);
        let uncompressed_bytes = word(16);
        let crc = u32::from_le_bytes(head[24..28].try_into().unwrap());
        let block_count = u32::from_le_bytes(head[28..32].try_into().unwrap()) as usize;
        if payload_len.div_ceil(u64::from(BLOCK_SIZE)) != block_count as u64 {
            return Err(corrupt("payload length disagrees with block count"));
        }
        // The claimed count must fit in the bytes actually present before
        // it sizes an allocation.
        if block_count > bytes.len().saturating_sub(pos + 32) / 8 {
            return Err(corrupt("truncated block list"));
        }
        let mut blocks = Vec::with_capacity(block_count);
        let mut p = pos + 32;
        for _ in 0..block_count {
            let ptr = bytes
                .get(p..p + 8)
                .ok_or_else(|| corrupt("truncated block list"))?;
            blocks.push(u64::from_le_bytes(ptr.try_into().unwrap()));
            p += 8;
        }
        Ok((
            Self {
                value_count,
                payload_len,
                uncompressed_bytes,
                crc,
                blocks,
            },
            p,
        ))
    }

    fn chunk_count(&self) -> usize {
        self.value_count.div_ceil(CHUNK_ROWS as u64) as usize
    }

    /// Rows in chunk `i`: full except possibly the last.
    fn chunk_rows(&self, i: usize) -> usize {
        (self.value_count as usize - i * CHUNK_ROWS).min(CHUNK_ROWS)
    }
}

/// Encodes `values` chunk by chunk with the cascade selector and writes
/// the MiniBlock payload across freshly allocated blocks.
pub fn write_segment(db: &mut Zu1File, values: &[u64]) -> Result<SegmentMeta> {
    let mut body = Vec::new();
    let chunk_count = values.len().div_ceil(CHUNK_ROWS);
    let mut ends = Vec::with_capacity(chunk_count);
    let mut fences = Vec::with_capacity(chunk_count);
    for chunk in values.chunks(CHUNK_ROWS) {
        enc::encode_auto(chunk, &mut body);
        if body.len() > u32::MAX as usize {
            return Err(ZuError::InvalidArgument(
                "segment body exceeds the u32 chunk index range".to_string(),
            ));
        }
        ends.push(body.len() as u32);
        fences.push(*chunk.last().unwrap());
    }
    let mut payload = Vec::with_capacity(4 + ends.len() * 12 + body.len());
    payload.extend_from_slice(&(ends.len() as u32).to_le_bytes());
    for e in &ends {
        payload.extend_from_slice(&e.to_le_bytes());
    }
    for f in &fences {
        payload.extend_from_slice(&f.to_le_bytes());
    }
    payload.extend_from_slice(&body);
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
        uncompressed_bytes: (values.len() * 8) as u64,
        crc,
        blocks,
    })
}

/// Reads a segment back, verifying the payload crc, and appends the
/// decoded values to `out`.
pub fn read_segment(db: &mut Zu1File, meta: &SegmentMeta, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
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
    let chunks = meta.chunk_count();
    let head = payload.get(..4).ok_or_else(|| corrupt("truncated index"))?;
    if u32::from_le_bytes(head.try_into().unwrap()) as usize != chunks {
        return Err(corrupt("chunk count disagrees with meta"));
    }
    let idx_len = 4 + chunks * 4;
    let index = payload
        .get(4..idx_len)
        .ok_or_else(|| corrupt("truncated index"))?;
    let fences = payload
        .get(idx_len..idx_len + chunks * 8)
        .ok_or_else(|| corrupt("truncated fences"))?;
    let body = &payload[idx_len + chunks * 8..];
    let mut prev = 0usize;
    for i in 0..chunks {
        let end = u32::from_le_bytes(index[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        if end < prev || end > body.len() {
            return Err(corrupt("chunk index not monotone"));
        }
        let before = out.len();
        enc::decode_any(&body[prev..end], meta.chunk_rows(i), out)?;
        if out.len() - before != meta.chunk_rows(i) {
            return Err(corrupt("chunk count disagrees with meta"));
        }
        let fence = u64::from_le_bytes(fences[i * 8..i * 8 + 8].try_into().unwrap());
        if *out.last().unwrap() != fence {
            return Err(corrupt("fence disagrees with chunk"));
        }
        prev = end;
    }
    if prev != body.len() {
        return Err(corrupt("trailing bytes after last chunk"));
    }
    Ok(())
}

/// Point access: appends `values[start..end]` to `out`, decoding only the
/// chunks that cover the range and reading only the bytes they occupy.
pub fn read_range(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    start: u64,
    end: u64,
    out: &mut Vec<u64>,
) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if start > end || end > meta.value_count {
        return Err(ZuError::InvalidArgument(format!(
            "range {start}..{end} out of 0..{}",
            meta.value_count
        )));
    }
    if start == end {
        return Ok(());
    }
    let chunks = meta.chunk_count();
    let body_off = 4 + chunks * 4 + chunks * 8;
    let first = start as usize / CHUNK_ROWS;
    let last = (end - 1) as usize / CHUNK_ROWS;
    // The index span needs the end of chunk `first - 1` to know where
    // chunk `first` starts; chunk 0 starts at the body.
    let lo_entry = first.saturating_sub(1);
    let span = read_payload_span(db, meta, 4 + lo_entry * 4, 4 + (last + 1) * 4)?;
    let ends: Vec<usize> = span
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        .collect();
    let ent = |chunk: usize| ends[chunk - lo_entry];
    let body_start = if first == 0 { 0 } else { ent(first - 1) };
    let body_end = ent(last);
    if body_start > body_end || body_off + body_end > meta.payload_len as usize {
        return Err(corrupt("chunk index not monotone"));
    }
    let bytes = read_payload_span(db, meta, body_off + body_start, body_off + body_end)?;
    let mut scratch = Vec::with_capacity(CHUNK_ROWS);
    let mut prev = body_start;
    for i in first..=last {
        let chunk_end = ent(i);
        if chunk_end < prev {
            return Err(corrupt("chunk index not monotone"));
        }
        scratch.clear();
        enc::decode_any(
            &bytes[prev - body_start..chunk_end - body_start],
            meta.chunk_rows(i),
            &mut scratch,
        )?;
        if scratch.len() != meta.chunk_rows(i) {
            return Err(corrupt("chunk count disagrees with meta"));
        }
        let lo = (start as usize).max(i * CHUNK_ROWS) - i * CHUNK_ROWS;
        let hi = (end as usize).min((i + 1) * CHUNK_ROWS) - i * CHUNK_ROWS;
        out.extend_from_slice(&scratch[lo..hi]);
        prev = chunk_end;
    }
    Ok(())
}

/// Membership probe: is `value` among `values[start..end)`? The rows in
/// that range must be sorted ascending, which CSR neighbor lists are.
/// The fences of chunks fully covered by the range are in-order samples
/// of it, so a binary search over them names the single chunk that could
/// hold `value`, and only that chunk is read and decoded: a probe
/// touches bytes on the order of one chunk regardless of the degree.
pub fn probe(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    start: u64,
    end: u64,
    value: u64,
) -> Result<bool> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if start > end || end > meta.value_count {
        return Err(ZuError::InvalidArgument(format!(
            "range {start}..{end} out of 0..{}",
            meta.value_count
        )));
    }
    if start == end {
        return Ok(false);
    }
    let chunks = meta.chunk_count();
    let fence_off = 4 + chunks * 4;
    let body_off = fence_off + chunks * 8;
    let first = start as usize / CHUNK_ROWS;
    let last = (end - 1) as usize / CHUNK_ROWS;
    // Chunks in [first, last) end inside the range, so their fences are
    // range values in ascending order. The target is the first chunk
    // whose fence admits `value`; failing all fences it is the tail
    // chunk, which is the only one whose fence lies outside the range.
    let target = if first == last {
        first
    } else {
        let span = read_payload_span(db, meta, fence_off + first * 8, fence_off + last * 8)?;
        let fences: Vec<u64> = span
            .chunks_exact(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        first + fences.partition_point(|&f| f < value)
    };
    let lo_entry = target.saturating_sub(1);
    let span = read_payload_span(db, meta, 4 + lo_entry * 4, 4 + (target + 1) * 4)?;
    let ends: Vec<usize> = span
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        .collect();
    let body_start = if target == 0 { 0 } else { ends[0] };
    let body_end = *ends.last().unwrap();
    if body_start > body_end || body_off + body_end > meta.payload_len as usize {
        return Err(corrupt("chunk index not monotone"));
    }
    let bytes = read_payload_span(db, meta, body_off + body_start, body_off + body_end)?;
    let mut scratch = Vec::with_capacity(CHUNK_ROWS);
    enc::decode_any(&bytes, meta.chunk_rows(target), &mut scratch)?;
    if scratch.len() != meta.chunk_rows(target) {
        return Err(corrupt("chunk count disagrees with meta"));
    }
    let lo = (start as usize).max(target * CHUNK_ROWS) - target * CHUNK_ROWS;
    let hi = (end as usize).min((target + 1) * CHUNK_ROWS) - target * CHUNK_ROWS;
    Ok(scratch[lo..hi].binary_search(&value).is_ok())
}

/// Reads payload bytes `[from, to)`, touching only the covering blocks.
fn read_payload_span(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    from: usize,
    to: usize,
) -> Result<Vec<u8>> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if from > to || to > meta.payload_len as usize {
        return Err(corrupt("span outside the payload"));
    }
    let block = BLOCK_SIZE as usize;
    let mut buf = Vec::with_capacity(to - from);
    let mut pos = from;
    while pos < to {
        let offset = pos % block;
        let len = (to - pos).min(block - offset);
        let ptr = *meta
            .blocks
            .get(pos / block)
            .ok_or_else(|| corrupt("span past the block list"))?;
        buf.extend_from_slice(&db.read_block_slice(ptr, offset, len)?);
        pos += len;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_multi_block_and_meta() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        // Wide random values force Plain, so 100k values span 4 blocks.
        let mut rng = 0xC0FFEEu64;
        let values: Vec<u64> = (0..100_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            })
            .collect();
        let meta = write_segment(&mut db, &values).unwrap();
        assert!(meta.blocks.len() >= 4, "got {} blocks", meta.blocks.len());

        let mut encoded = Vec::new();
        meta.encode(&mut encoded);
        let (decoded, end) = SegmentMeta::decode(&encoded, 0).unwrap();
        assert_eq!(decoded, meta);
        assert_eq!(end, encoded.len());

        let mut out = Vec::new();
        read_segment(&mut db, &meta, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn sorted_ids_stay_small_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..200_000u64).map(|i| i * 3).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        assert_eq!(meta.blocks.len(), 1, "delta packed ids fit one block");
        let mut out = Vec::new();
        read_segment(&mut db, &meta, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn point_reads_match_full_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        // Mixed-shape values across many chunks so different chunks pick
        // different encodings.
        let mut rng = 0xF00Du64;
        let values: Vec<u64> = (0..10_000u64)
            .map(|i| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                match (i / CHUNK_ROWS as u64) % 3 {
                    0 => i * 5,
                    1 => rng,
                    _ => rng % 4,
                }
            })
            .collect();
        let meta = write_segment(&mut db, &values).unwrap();
        let ranges = [
            (0u64, 1u64),
            (0, values.len() as u64),
            (1023, 1025),
            (1024, 2048),
            (2047, 2049),
            (5000, 5000),
            (9_999, 10_000),
            (500, 7_777),
        ];
        for (s, e) in ranges {
            let mut got = Vec::new();
            read_range(&mut db, &meta, s, e, &mut got).unwrap();
            assert_eq!(got, values[s as usize..e as usize], "range {s}..{e}");
        }
        let mut out = Vec::new();
        assert!(read_range(&mut db, &meta, 5, 4, &mut out).is_err());
        assert!(read_range(&mut db, &meta, 0, values.len() as u64 + 1, &mut out).is_err());
    }

    #[test]
    fn corrupt_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[10] ^= 0xFF;
        db.write_block(meta.blocks[0], &block).unwrap();
        let mut out = Vec::new();
        assert!(read_segment(&mut db, &meta, &mut out).is_err());
    }

    #[test]
    fn corrupt_index_cannot_panic_point_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).map(|i| i * 3).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        // Flip the index entry of chunk 2 to a huge end offset; the point
        // path skips the crc, so it must fail on its own bounds checks.
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[4 + 2 * 4..4 + 3 * 4].copy_from_slice(&u32::MAX.to_le_bytes());
        db.write_block(meta.blocks[0], &block).unwrap();
        let mut out = Vec::new();
        assert!(read_range(&mut db, &meta, 2048, 2050, &mut out).is_err());
        // A non-monotone entry (end below the previous chunk's end).
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[4 + 2 * 4..4 + 3 * 4].copy_from_slice(&0u32.to_le_bytes());
        db.write_block(meta.blocks[0], &block).unwrap();
        assert!(read_range(&mut db, &meta, 2048, 2050, &mut out).is_err());
    }

    #[test]
    fn probe_matches_binary_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        // Sorted evens across several chunks, so odds are always absent
        // and every present value has known neighbors on both sides.
        let values: Vec<u64> = (0..5000u64).map(|i| i * 2).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        let ranges = [
            (0u64, values.len() as u64),
            (0, 1),
            (1023, 1025),
            (1024, 2048),
            (500, 4500),
            (4999, 5000),
        ];
        for (s, e) in ranges {
            for v in [0u64, 1, 2046, 2047, 2048, 4096, 8998, 9998, 9999, 100_000] {
                let want = values[s as usize..e as usize].binary_search(&v).is_ok();
                let got = probe(&mut db, &meta, s, e, v).unwrap();
                assert_eq!(got, want, "range {s}..{e} value {v}");
            }
        }
        let mut out = Vec::new();
        assert!(probe(&mut db, &meta, 5, 4, 0).is_err());
        assert!(probe(&mut db, &meta, 0, values.len() as u64 + 1, 0).is_err());
        assert!(!probe(&mut db, &meta, 7, 7, 14).unwrap());
        read_segment(&mut db, &meta, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn corrupt_fence_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).map(|i| i * 3).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        // Flip a bit inside chunk 2's fence. The crc catches it on the
        // full path, so rewrite the crc too and rely on the fence cross
        // check instead, which is what protects the probe path.
        let chunks = values.len().div_ceil(CHUNK_ROWS);
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[4 + chunks * 4 + 2 * 8] ^= 0xFF;
        db.write_block(meta.blocks[0], &block).unwrap();
        let mut patched = meta.clone();
        patched.crc = crc32c::crc32c(&block[..patched.payload_len as usize]);
        let mut out = Vec::new();
        let err = read_segment(&mut db, &patched, &mut out).unwrap_err();
        assert!(format!("{err}").contains("fence"));
    }

    #[test]
    fn hostile_meta_block_count_rejected() {
        // A 32 byte header whose payload length and block count agree
        // with each other but not with the bytes present: the claimed
        // list must fail the size check before it sizes an allocation.
        let block_count = 0x1000_0000u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&100u64.to_le_bytes());
        bytes.extend_from_slice(&(u64::from(block_count) * u64::from(BLOCK_SIZE)).to_le_bytes());
        bytes.extend_from_slice(&800u64.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&block_count.to_le_bytes());
        let err = SegmentMeta::decode(&bytes, 0).unwrap_err();
        assert!(format!("{err}").contains("truncated block list"));
    }

    #[test]
    fn empty_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let meta = write_segment(&mut db, &[]).unwrap();
        assert_eq!(meta.blocks.len(), 1, "even empty carries the chunk count");
        let mut out = Vec::new();
        read_segment(&mut db, &meta, &mut out).unwrap();
        assert!(out.is_empty());
        read_range(&mut db, &meta, 0, 0, &mut out).unwrap();
        assert!(out.is_empty());
    }
}
