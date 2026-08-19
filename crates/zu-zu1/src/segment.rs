//! Column segments in the MiniBlock structural layout of
//! `docs/04-storage-zu1-format.md` §3: values packed in chunks of 1024,
//! each chunk an independently decodable `encode_auto` cascade, behind a
//! trailer that says where each chunk's bytes are. A point read decodes
//! only the chunks covering the wanted rows and touches bytes on the
//! order of the chunk, not the segment.
//!
//! Payload layout, all little-endian: the body of chunk cascades first,
//! then `chunk_count: u32`, then one entry per chunk holding
//! `start: u32`, `len: u32` and one word more, which for MiniBlock is
//! the fence, the chunk's last value. The encoding id travels inside
//! each chunk, so the reader needs no side channel beyond the
//! `SegmentMeta`.
//!
//! The index sits after the body rather than before it, and a chunk is
//! addressed rather than ordered, and both of those are for the same
//! reason. A write that changes one chunk puts its new bytes on the end
//! of the body and repoints the entry, so every byte before that lands
//! at the offset it already had, in the block that already held it, and
//! the rewrite hands those blocks on by pointer instead of copying
//! them. What it leaves behind is a hole; `live_bytes` counts what is
//! still pointed at and a rewrite that finds the holes are most of the
//! body lays the segment out again without them.
//!
//! The fences are what make [`probe`] cheap: inside any sorted row range
//! the fence of a fully covered chunk is an in-order sample, so a binary
//! search over fences names the one chunk that could hold a value and the
//! probe decodes that chunk alone. They cost 8 bytes per 1024 values,
//! about 0.06 bits per value.
//!
//! Metas serialize into meta-block chains with a fixed layout:
//! `value_count: u64`, `payload_len: u64`, `uncompressed_bytes: u64`,
//! `min: u64`, `max: u64`, `live_bytes: u64`, `block_count: u32`,
//! `structural: u8`, then one `u64` per block pointer and one `u32`
//! checksum per block. The structural byte names the payload layout,
//! MiniBlock here or FullZip in `crate::fullzip`, and an unknown id is
//! an error naming it (docs/04 §10). The min and max are the segment's
//! zone map (`docs/04` §6): [`probe`] answers absent for any value
//! outside them without touching the payload. They are bounds rather
//! than a census, because a rewrite widens them by what it wrote and
//! cannot narrow them by what it replaced, so the full scan checks the
//! values lie inside them rather than reach them.
//!
//! The checksums are per block, over the payload bytes a block holds and
//! not its padding, and they are verified by the full scan path and
//! `zu verify`. One checksum over the payload would make every rewrite
//! read every byte it was trying not to touch. The point path skips them
//! by design (checking a block would mean reading 256 KiB to answer for
//! one chunk); it bounds every access by the meta and rejects extents
//! outside the body, truncated chunks, and count mismatches instead.

use zu_common::{Result, ZuError};
use zu_encoding::segment as enc;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::BLOCK_SIZE;
use crate::cache::{DecodedPool, SegmentBytes};
use crate::file::{BlockPtr, Zu1File};

/// Rows per MiniBlock chunk, the unit of point access.
pub const CHUNK_ROWS: usize = 1024;

/// Structural layout of a segment payload (docs/04 §3): MiniBlock packs
/// fixed-width values in 1024-row cascade chunks, FullZip zips
/// variable-width values with their lengths (`crate::fullzip`). The ids
/// are format-stable; readers reject any other value by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structural {
    MiniBlock = 0,
    FullZip = 1,
}

/// Bytes one chunk takes in the trailer: where its body bytes start,
/// how many of them there are, and one word more, which is the fence
/// for a MiniBlock and the unpacked size for a FullZip.
pub(crate) const ENTRY_BYTES: usize = 16;

/// Where one chunk's bytes are and what the trailer says about it.
///
/// A chunk is addressed rather than ordered, which is the whole of why
/// a write into the middle of a segment does not move the bytes around
/// it: the new bytes go on the end of the body and the entry is
/// repointed at them, leaving the old ones where they lie as garbage
/// for the next compaction to drop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    pub start: u32,
    pub len: u32,
    /// The chunk's last value for a MiniBlock, which is its fence; the
    /// unpacked size of the chunk for a FullZip.
    pub aux: u64,
}

/// Location and integrity data for one stored segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentMeta {
    pub value_count: u64,
    pub payload_len: u64,
    pub uncompressed_bytes: u64,
    /// Zone map: a lower bound on the values in the segment, 0 when
    /// empty. It is the smallest value in a segment that was written
    /// whole, and a bound rather than the minimum in one a rewrite
    /// widened, because a rewrite knows what it put in and not what it
    /// took out.
    pub min: u64,
    /// Zone map: an upper bound on the values, on the same terms.
    pub max: u64,
    /// Body bytes some chunk still points at. The difference between
    /// this and the body length is what a chunk rewrite left behind,
    /// and it is what says when a segment is worth compacting.
    pub live_bytes: u64,
    pub structural: Structural,
    /// The writer saw the values in ascending order, which upgrades the
    /// fence array to a per-chunk zone map: chunk `i` holds values in
    /// `fences[i-1]..=fences[i]` (`min..=fences[0]` for the first), so
    /// a range scan can skip chunks without reading them. Unsorted
    /// segments keep only the segment-level min and max.
    pub sorted: bool,
    pub blocks: Vec<BlockPtr>,
    /// One crc32c per block, over the payload bytes that block holds
    /// and not over its zero padding.
    ///
    /// One checksum over the whole payload would make every rewrite
    /// read every byte it was trying not to touch, which is the cost
    /// this format exists to remove. Per block, a rewrite checksums
    /// what it wrote and inherits the rest.
    pub crcs: Vec<u32>,
}

impl SegmentMeta {
    /// Serialized size in bytes.
    pub fn encoded_len(&self) -> usize {
        53 + self.blocks.len() * 12
    }

    /// Appends the meta to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.value_count.to_le_bytes());
        out.extend_from_slice(&self.payload_len.to_le_bytes());
        out.extend_from_slice(&self.uncompressed_bytes.to_le_bytes());
        out.extend_from_slice(&self.min.to_le_bytes());
        out.extend_from_slice(&self.max.to_le_bytes());
        out.extend_from_slice(&self.live_bytes.to_le_bytes());
        out.extend_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        // The structural byte carries the sorted flag in bit 1.
        out.push(self.structural as u8 | (u8::from(self.sorted) << 1));
        for b in &self.blocks {
            out.extend_from_slice(&b.to_le_bytes());
        }
        for c in &self.crcs {
            out.extend_from_slice(&c.to_le_bytes());
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
            .get(pos..pos + 53)
            .ok_or_else(|| corrupt("truncated header"))?;
        let word = |i: usize| u64::from_le_bytes(head[i..i + 8].try_into().unwrap());
        let value_count = word(0);
        let payload_len = word(8);
        let uncompressed_bytes = word(16);
        let min = word(24);
        let max = word(32);
        let live_bytes = word(40);
        let block_count = u32::from_le_bytes(head[48..52].try_into().unwrap()) as usize;
        let structural = match head[52] & !2 {
            0 => Structural::MiniBlock,
            1 => Structural::FullZip,
            _ => {
                return Err(ZuError::Unsupported {
                    what: "structural layout",
                    id: u32::from(head[52]),
                });
            }
        };
        let sorted = head[52] & 2 != 0;
        if min > max {
            return Err(corrupt("zone min above max"));
        }
        if payload_len.div_ceil(u64::from(BLOCK_SIZE)) != block_count as u64 {
            return Err(corrupt("payload length disagrees with block count"));
        }
        // The claimed count must fit in the bytes actually present before
        // it sizes an allocation.
        if block_count > bytes.len().saturating_sub(pos + 53) / 12 {
            return Err(corrupt("truncated block list"));
        }
        let mut blocks = Vec::with_capacity(block_count);
        let mut p = pos + 53;
        for _ in 0..block_count {
            let ptr = bytes
                .get(p..p + 8)
                .ok_or_else(|| corrupt("truncated block list"))?;
            blocks.push(u64::from_le_bytes(ptr.try_into().unwrap()));
            p += 8;
        }
        let mut crcs = Vec::with_capacity(block_count);
        for _ in 0..block_count {
            let c = bytes
                .get(p..p + 4)
                .ok_or_else(|| corrupt("truncated block list"))?;
            crcs.push(u32::from_le_bytes(c.try_into().unwrap()));
            p += 4;
        }
        Ok((
            Self {
                value_count,
                payload_len,
                uncompressed_bytes,
                min,
                max,
                live_bytes,
                structural,
                sorted,
                blocks,
                crcs,
            },
            p,
        ))
    }

    pub(crate) fn chunk_count(&self) -> usize {
        self.value_count.div_ceil(CHUNK_ROWS as u64) as usize
    }

    /// Rows in chunk `i`: full except possibly the last.
    pub(crate) fn chunk_rows(&self, i: usize) -> usize {
        (self.value_count as usize - i * CHUNK_ROWS).min(CHUNK_ROWS)
    }

    /// Bytes the trailer takes: a chunk count and an entry per chunk.
    pub(crate) fn trailer_len(&self) -> usize {
        4 + self.chunk_count() * ENTRY_BYTES
    }

    /// Where the trailer starts, which is one past the last body byte.
    pub(crate) fn body_len(&self) -> Result<usize> {
        (self.payload_len as usize)
            .checked_sub(self.trailer_len())
            .ok_or_else(|| ZuError::Corrupt {
                what: "segment",
                detail: "payload shorter than its trailer".to_string(),
            })
    }

    /// The byte the payload of block `i` ends at inside that block,
    /// which is the whole block except for the last one.
    fn block_fill(&self, i: usize) -> usize {
        (self.payload_len as usize - i * BLOCK_SIZE as usize).min(BLOCK_SIZE as usize)
    }
}

/// Lays a trailer out after `body`.
pub(crate) fn append_trailer(payload: &mut Vec<u8>, entries: &[Entry]) {
    payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        payload.extend_from_slice(&e.start.to_le_bytes());
        payload.extend_from_slice(&e.len.to_le_bytes());
        payload.extend_from_slice(&e.aux.to_le_bytes());
    }
}

/// Reads `count` entries out of trailer bytes that start with the
/// chunk count.
pub(crate) fn parse_trailer(bytes: &[u8], count: usize) -> Result<Vec<Entry>> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    let head = bytes.get(..4).ok_or_else(|| corrupt("truncated trailer"))?;
    if u32::from_le_bytes(head.try_into().unwrap()) as usize != count {
        return Err(corrupt("chunk count disagrees with meta"));
    }
    let body = bytes
        .get(4..4 + count * ENTRY_BYTES)
        .ok_or_else(|| corrupt("truncated trailer"))?;
    Ok(body.chunks_exact(ENTRY_BYTES).map(entry_at).collect())
}

/// One entry off its 16 bytes.
pub(crate) fn entry_at(e: &[u8]) -> Entry {
    Entry {
        start: u32::from_le_bytes(e[0..4].try_into().unwrap()),
        len: u32::from_le_bytes(e[4..8].try_into().unwrap()),
        aux: u64::from_le_bytes(e[8..16].try_into().unwrap()),
    }
}

/// The body span of one entry, bounds checked against the body.
pub(crate) fn entry_span(e: Entry, body_len: usize) -> Result<std::ops::Range<usize>> {
    let end = (e.start as usize)
        .checked_add(e.len as usize)
        .filter(|&end| end <= body_len)
        .ok_or_else(|| ZuError::Corrupt {
            what: "segment",
            detail: "chunk extent outside the body".to_string(),
        })?;
    Ok(e.start as usize..end)
}

/// Encodes `values` chunk by chunk with the cascade selector and writes
/// the MiniBlock payload across freshly allocated blocks.
pub fn write_segment(db: &mut Zu1File, values: &[u64]) -> Result<SegmentMeta> {
    let mut payload = Vec::new();
    let mut entries = Vec::with_capacity(values.len().div_ceil(CHUNK_ROWS));
    for chunk in values.chunks(CHUNK_ROWS) {
        let start = payload.len();
        enc::encode_auto(chunk, &mut payload);
        if payload.len() > u32::MAX as usize {
            return Err(ZuError::InvalidArgument(
                "segment body exceeds the u32 chunk index range".to_string(),
            ));
        }
        entries.push(Entry {
            start: start as u32,
            len: (payload.len() - start) as u32,
            aux: *chunk.last().unwrap(),
        });
    }
    let live = payload.len() as u64;
    append_trailer(&mut payload, &entries);
    let (blocks, crcs, payload_len) = store_payload(db, &[], &[], &payload)?;
    Ok(SegmentMeta {
        value_count: values.len() as u64,
        payload_len,
        uncompressed_bytes: (values.len() * 8) as u64,
        min: values.iter().copied().min().unwrap_or(0),
        max: values.iter().copied().max().unwrap_or(0),
        live_bytes: live,
        structural: Structural::MiniBlock,
        sorted: values.is_sorted(),
        blocks,
        crcs,
    })
}

/// Writes a payload whose first `keep.len()` blocks already hold, on
/// disk, exactly the bytes they should, and whose remainder is `tail`
/// starting at `keep.len() * BLOCK_SIZE`.
///
/// This is the whole of the block reuse and it is why the body of a
/// segment is laid out before its index rather than after it. A write
/// that only put bytes on the end of the body leaves every block
/// before the one it landed in byte for byte as it was, and those
/// blocks come across by pointer: no read, no write, no allocation and
/// no free.
pub(crate) fn store_payload(
    db: &mut Zu1File,
    keep: &[BlockPtr],
    keep_crcs: &[u32],
    tail: &[u8],
) -> Result<(Vec<BlockPtr>, Vec<u32>, u64)> {
    let mut blocks = keep.to_vec();
    let mut crcs = keep_crcs.to_vec();
    let mut block = vec![0u8; BLOCK_SIZE as usize];
    for part in tail.chunks(BLOCK_SIZE as usize) {
        let ptr = db.allocate_block();
        block[..part.len()].copy_from_slice(part);
        block[part.len()..].fill(0);
        db.write_block(ptr, &block)?;
        blocks.push(ptr);
        // The checksum covers the payload bytes the block holds and not
        // the padding after them, so it is the same value whichever
        // block a span of payload ends up in.
        crcs.push(crc32c::crc32c(part));
    }
    let payload_len = (keep.len() * BLOCK_SIZE as usize + tail.len()) as u64;
    Ok((blocks, crcs, payload_len))
}

/// Writes the segment `old` becomes once `updates` are applied to the
/// rows it holds and `appended` goes on the end of it.
///
/// What this does not do is the point of it. It does not decode the
/// column, it does not copy the column, and it does not write the
/// column. It reads the trailer, decodes the chunks an update landed
/// in and the one the appends grew, encodes those, puts the new bytes
/// on the end of the body and repoints their entries at them. Every
/// other chunk keeps the bytes it had, at the offset it had them, in
/// the block that held them, and that block comes across by pointer.
///
/// So an append of k rows to a column of n costs k rows of encoding and
/// two blocks of writing, whatever n is, and a write into one cell
/// costs one chunk, wherever in the column that cell is.
///
/// The old chunk's bytes stay in the body as a hole. `live_bytes`
/// counts what is still pointed at, and when the holes are more than
/// half the body the segment is laid out again without them, which is
/// one pass over it amortised against having moved nothing on every
/// write before.
pub fn rewrite_segment(
    db: &mut Zu1File,
    old: &SegmentMeta,
    updates: &BTreeMap<u64, u64>,
    appended: &[u64],
) -> Result<SegmentMeta> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if old.structural != Structural::MiniBlock {
        return Err(corrupt("MiniBlock rewrite given a FullZip segment"));
    }
    let base = old.value_count;
    if let Some((&row, _)) = updates.iter().next_back()
        && row >= base
    {
        return Err(ZuError::InvalidArgument(format!(
            "update at row {row} past the {base} rows the segment holds"
        )));
    }
    if updates.is_empty() && appended.is_empty() {
        return Ok(old.clone());
    }
    let new_count = base + appended.len() as u64;
    let old_chunks = old.chunk_count();
    let new_chunks = (new_count as usize).div_ceil(CHUNK_ROWS);
    let body_len = old.body_len()?;
    let mut entries = read_trailer(db, old)?;
    for &e in &entries {
        entry_span(e, body_len)?;
    }
    entries.resize(new_chunks, Entry::default());

    // The chunks that have to go back through the selector: the ones an
    // update landed in, every chunk past the old end, and the old tail
    // when the appends filled it out, since short of the row count
    // nothing about a chunk is the same.
    let rows_in = |i: usize| ((i + 1) * CHUNK_ROWS).min(new_count as usize) - i * CHUNK_ROWS;
    let mut dirty: BTreeSet<usize> = updates.keys().map(|&r| r as usize / CHUNK_ROWS).collect();
    dirty.extend(old_chunks..new_chunks);
    if old_chunks > 0 && old.chunk_rows(old_chunks - 1) != rows_in(old_chunks - 1) {
        dirty.insert(old_chunks - 1);
    }

    let mut live = old.live_bytes;
    let mut fresh = Vec::new();
    let mut scratch = Vec::with_capacity(CHUNK_ROWS);
    for &i in &dirty {
        let lo = i * CHUNK_ROWS;
        let rows = rows_in(i);
        scratch.clear();
        if i < old_chunks {
            let span = entry_span(entries[i], body_len)?;
            let bytes = read_payload_span(db, old, span.start, span.end)?;
            enc::decode_any(&bytes, old.chunk_rows(i), &mut scratch)?;
            if scratch.len() != old.chunk_rows(i) {
                return Err(corrupt("chunk count disagrees with meta"));
            }
            live -= u64::from(entries[i].len);
        }
        for row in lo + scratch.len()..lo + rows {
            scratch.push(appended[row - base as usize]);
        }
        for (&row, &value) in updates.range(lo as u64..(lo + rows) as u64) {
            scratch[row as usize - lo] = value;
        }
        let start = body_len + fresh.len();
        enc::encode_auto(&scratch, &mut fresh);
        let len = body_len + fresh.len() - start;
        if start + len > u32::MAX as usize {
            return Err(ZuError::InvalidArgument(
                "segment body exceeds the u32 chunk index range".to_string(),
            ));
        }
        entries[i] = Entry {
            start: start as u32,
            len: len as u32,
            aux: *scratch.last().unwrap(),
        };
        live += len as u64;
    }

    // The zone is a bound rather than a census. A rewrite knows what it
    // put in and not what it took out, so it widens and never narrows,
    // and a column that was written over many times ends up with a
    // looser bound than a fresh one would have. That costs pruning and
    // never correctness. An empty base has no bound worth keeping, so
    // it starts again instead of flooring the zone at zero.
    let (mut min, mut max) = match base {
        0 => (u64::MAX, 0),
        _ => (old.min, old.max),
    };
    for &v in appended.iter().chain(updates.values()) {
        min = min.min(v);
        max = max.max(v);
    }
    if new_count == 0 {
        min = 0;
    }
    // Ascending order survives an append that carries on where the
    // segment left off, and nothing else: an update can put any value
    // into any row and this does not read the rows it did not write.
    let sorted = old.sorted
        && updates.is_empty()
        && appended.is_sorted()
        && (base == 0 || appended.first().is_none_or(|&v| v >= old.max));

    let new_body_len = body_len + fresh.len();
    let head = SegmentMeta {
        value_count: new_count,
        payload_len: 0,
        uncompressed_bytes: new_count * 8,
        min,
        max,
        live_bytes: live,
        structural: Structural::MiniBlock,
        sorted,
        blocks: Vec::new(),
        crcs: Vec::new(),
    };
    if (new_body_len as u64 - live) * 2 > new_body_len as u64 {
        return compact_segment(db, old, head, &entries, &fresh, body_len);
    }

    // Everything below the old body length is where it was, so the
    // blocks that lie wholly inside it come across untouched and the
    // write starts at the first one that does not.
    let keep = body_len / BLOCK_SIZE as usize;
    let mut tail = Vec::with_capacity(new_body_len - keep * BLOCK_SIZE as usize + 4);
    let from = keep * BLOCK_SIZE as usize;
    if from < body_len {
        tail.extend_from_slice(&read_payload_span(db, old, from, body_len)?);
    }
    tail.extend_from_slice(&fresh);
    append_trailer(&mut tail, &entries);
    let (blocks, crcs, payload_len) =
        store_payload(db, &old.blocks[..keep], &old.crcs[..keep], &tail)?;
    for &ptr in &old.blocks[keep..] {
        db.free_block(ptr)?;
    }
    // A pooled decode is keyed on the segment's first block, and that
    // block has just stopped naming what it named. Nothing rewrote it,
    // so nothing else drops the key.
    if keep > 0 {
        db.forget_segment(old.blocks[0]);
    }
    Ok(SegmentMeta {
        payload_len,
        blocks,
        crcs,
        ..head
    })
}

/// Lays a segment out again in row order with the holes taken out.
///
/// `entries` names the chunks as they stand, pointing either into the
/// old body or into `fresh`, which sits directly after it. Nothing is
/// decoded: a chunk's bytes are already an encoding of exactly its
/// values, so this is a copy and the selector is not asked anything.
fn compact_segment(
    db: &mut Zu1File,
    old: &SegmentMeta,
    head: SegmentMeta,
    entries: &[Entry],
    fresh: &[u8],
    body_len: usize,
) -> Result<SegmentMeta> {
    let payload = read_payload_verified(db, old)?;
    let body = &payload[..body_len];
    let mut out = Vec::with_capacity(head.live_bytes as usize + 4 + entries.len() * ENTRY_BYTES);
    let mut packed = Vec::with_capacity(entries.len());
    for &e in entries {
        let start = out.len();
        match e.start as usize >= body_len {
            true => {
                let at = e.start as usize - body_len;
                let end = at
                    .checked_add(e.len as usize)
                    .filter(|&end| end <= fresh.len())
                    .ok_or_else(|| ZuError::Corrupt {
                        what: "segment",
                        detail: "chunk extent outside the body".to_string(),
                    })?;
                out.extend_from_slice(&fresh[at..end]);
            }
            false => out.extend_from_slice(&body[entry_span(e, body_len)?]),
        }
        packed.push(Entry {
            start: start as u32,
            len: (out.len() - start) as u32,
            aux: e.aux,
        });
    }
    let live = out.len() as u64;
    append_trailer(&mut out, &packed);
    let (blocks, crcs, payload_len) = store_payload(db, &[], &[], &out)?;
    for &ptr in &old.blocks {
        db.free_block(ptr)?;
    }
    Ok(SegmentMeta {
        payload_len,
        live_bytes: live,
        blocks,
        crcs,
        ..head
    })
}

/// Reads a segment's payload back off its blocks, verifying each block
/// against its own checksum on the way past.
pub(crate) fn read_payload_verified(db: &mut Zu1File, meta: &SegmentMeta) -> Result<Vec<u8>> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if meta.crcs.len() != meta.blocks.len() {
        return Err(corrupt("checksum list disagrees with the block list"));
    }
    // The claimed length only seeds the reservation; growth past the cap
    // is bounded by the block reads, which fail on the first bad pointer.
    let mut payload = Vec::with_capacity((meta.payload_len as usize).min(1 << 22));
    for (i, &ptr) in meta.blocks.iter().enumerate() {
        if i * BLOCK_SIZE as usize >= meta.payload_len as usize {
            break;
        }
        let block = db.pin_block(ptr)?;
        let part = &block[..meta.block_fill(i).min(block.len())];
        if crc32c::crc32c(part) != meta.crcs[i] {
            return Err(corrupt("payload crc mismatch"));
        }
        payload.extend_from_slice(part);
    }
    if payload.len() != meta.payload_len as usize {
        return Err(corrupt("payload shorter than meta claims"));
    }
    Ok(payload)
}

fn read_payload(db: &mut Zu1File, meta: &SegmentMeta) -> Result<Vec<u8>> {
    if meta.structural != Structural::MiniBlock {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "MiniBlock reader given a FullZip segment".to_string(),
        });
    }
    read_payload_verified(db, meta)
}

/// Reads a segment's trailer and nothing else, which is an entry per
/// chunk, so a reader that wants one chunk reads kilobytes rather than
/// the column.
pub(crate) fn read_trailer(db: &mut Zu1File, meta: &SegmentMeta) -> Result<Vec<Entry>> {
    let body_len = meta.body_len()?;
    let bytes = read_payload_span(db, meta, body_len, meta.payload_len as usize)?;
    parse_trailer(&bytes, meta.chunk_count())
}

/// The body and the entries of a payload that has been read whole.
pub(crate) fn body_and_entries<'p>(
    meta: &SegmentMeta,
    payload: &'p [u8],
) -> Result<(&'p [u8], Vec<Entry>)> {
    let body_len = meta.body_len()?;
    let body = payload.get(..body_len).ok_or_else(|| ZuError::Corrupt {
        what: "segment",
        detail: "payload shorter than its body".to_string(),
    })?;
    let entries = parse_trailer(&payload[body_len..], meta.chunk_count())?;
    Ok((body, entries))
}

/// Reads the trailer entries for chunks `first..=last` and nothing
/// else. Entries are fixed width and laid out in chunk order, so any
/// run of them is one span read of `16 * (last - first + 1)` bytes
/// however the chunks themselves are scattered through the body.
pub(crate) fn read_entry_run(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    first: usize,
    last: usize,
) -> Result<Vec<Entry>> {
    if first > last || last >= meta.chunk_count() {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "chunk run outside the segment".to_string(),
        });
    }
    let base = meta.body_len()? + 4;
    let bytes = read_payload_span(
        db,
        meta,
        base + first * ENTRY_BYTES,
        base + (last + 1) * ENTRY_BYTES,
    )?;
    Ok(bytes.chunks_exact(ENTRY_BYTES).map(entry_at).collect())
}

/// Reads the bytes of one chunk and decodes them into `out`, which is
/// cleared first. This is the only read a point access makes into the
/// body, and it is the size of the chunk and not of the segment.
fn decode_extent(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    e: Entry,
    rows: usize,
    out: &mut Vec<u64>,
) -> Result<()> {
    let span = entry_span(e, meta.body_len()?)?;
    let bytes = read_payload_span(db, meta, span.start, span.end)?;
    out.clear();
    enc::decode_any(&bytes, rows, out)?;
    if out.len() != rows {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "chunk count disagrees with meta".to_string(),
        });
    }
    Ok(())
}

/// Reads a segment back, verifying every block against its checksum,
/// and appends the decoded values to `out`.
pub fn read_segment(db: &mut Zu1File, meta: &SegmentMeta, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    let payload = read_payload(db, meta)?;
    let (body, entries) = body_and_entries(meta, &payload)?;
    let first_out = out.len();
    let mut live = 0u64;
    for (i, &e) in entries.iter().enumerate() {
        let span = entry_span(e, body.len())?;
        let before = out.len();
        live += u64::from(e.len);
        enc::decode_any(&body[span], meta.chunk_rows(i), out)?;
        if out.len() - before != meta.chunk_rows(i) {
            return Err(corrupt("chunk count disagrees with meta"));
        }
        if *out.last().unwrap() != e.aux {
            return Err(corrupt("fence disagrees with chunk"));
        }
    }
    if live != meta.live_bytes {
        return Err(corrupt("live bytes disagree with the chunk extents"));
    }
    // The zone map is what lets probes skip segments unread, so the full
    // scan holds it to the values the same way it holds every fence. It
    // is a bound rather than a census, because a rewrite widens it by
    // what it wrote and cannot narrow it by what it replaced, so what
    // is checked is that the values lie inside it.
    if meta.value_count > 0 {
        let decoded = &out[first_out..];
        let lo = decoded.iter().copied().min().unwrap();
        let hi = decoded.iter().copied().max().unwrap();
        if lo < meta.min || hi > meta.max {
            return Err(corrupt("zone disagrees with values"));
        }
    }
    Ok(())
}

/// Reads a whole segment through `pool`, decoding it at most once per
/// pooled lifetime. The key is the segment's first block pointer, which
/// every segment has since even an empty one carries its chunk count,
/// and which is unique per committed segment version.
pub fn read_segment_pooled(
    db: &mut Zu1File,
    pool: &DecodedPool<Vec<u64>>,
    meta: &SegmentMeta,
) -> Result<Arc<Vec<u64>>> {
    let key = meta.blocks[0];
    if let Some(values) = pool.get(key) {
        return Ok(values);
    }
    let mut values = Vec::with_capacity(meta.value_count as usize);
    read_segment(db, meta, &mut values)?;
    let values = Arc::new(values);
    pool.insert(key, Arc::clone(&values));
    Ok(values)
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
    if meta.structural != Structural::MiniBlock {
        return Err(corrupt("MiniBlock reader given a FullZip segment"));
    }
    if start == end {
        return Ok(());
    }
    let first = start as usize / CHUNK_ROWS;
    let last = (end - 1) as usize / CHUNK_ROWS;
    // The entries of the covered chunks come across in one span read.
    // Their bodies do not, because a chunk a rewrite moved sits at the
    // tail rather than beside its neighbours, so each one is read at
    // the extent its entry names.
    let entries = read_entry_run(db, meta, first, last)?;
    let mut scratch = Vec::with_capacity(CHUNK_ROWS);
    for i in first..=last {
        decode_extent(
            db,
            meta,
            entries[i - first],
            meta.chunk_rows(i),
            &mut scratch,
        )?;
        let lo = (start as usize).max(i * CHUNK_ROWS) - i * CHUNK_ROWS;
        let hi = (end as usize).min((i + 1) * CHUNK_ROWS) - i * CHUNK_ROWS;
        out.extend_from_slice(&scratch[lo..hi]);
    }
    Ok(())
}

/// Membership probe: is `value` among `values[start..end)`? The rows in
/// that range must be sorted ascending, which CSR neighbor lists are.
/// The fences of chunks fully covered by the range are in-order samples
/// of it, so a binary search over them names the single chunk that could
/// hold `value`, and only that chunk is read and decoded: a probe
/// touches bytes on the order of one chunk regardless of the degree.
/// A value outside the segment's zone map answers absent from the meta
/// alone, without reading the payload at all.
pub fn probe(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    start: u64,
    end: u64,
    value: u64,
) -> Result<bool> {
    Ok(locate(db, meta, start, end, value)?.is_some())
}

/// Where `value` sits in `values[start..end)`, as a position in the
/// whole segment, and `None` when the range does not hold it. Same
/// search and same cost as [`probe`], which is written in terms of it.
///
/// The position is what an edge property column is indexed by: a
/// neighbor list is one node's slice of the segment, so the slot a
/// destination lands in is the edge's place in the load order, and the
/// caller adds the group's base to get the ordinal.
pub fn locate(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    start: u64,
    end: u64,
    value: u64,
) -> Result<Option<u64>> {
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
    if meta.structural != Structural::MiniBlock {
        return Err(corrupt("MiniBlock reader given a FullZip segment"));
    }
    if start == end {
        return Ok(None);
    }
    // The zone covers the whole segment, so a value outside it is absent
    // from every row range.
    if value < meta.min || value > meta.max {
        return Ok(None);
    }
    let first = start as usize / CHUNK_ROWS;
    let last = (end - 1) as usize / CHUNK_ROWS;
    // Chunks in [first, last) end inside the range, so their fences are
    // range values in ascending order. The target is the first chunk
    // whose fence admits `value`; failing all fences it is the tail
    // chunk, which is the only one whose fence lies outside the range.
    let entries = read_entry_run(db, meta, first, last)?;
    let target = if first == last {
        first
    } else {
        first + entries[..last - first].partition_point(|e| e.aux < value)
    };
    let mut scratch = Vec::with_capacity(CHUNK_ROWS);
    decode_extent(
        db,
        meta,
        entries[target - first],
        meta.chunk_rows(target),
        &mut scratch,
    )?;
    let lo = (start as usize).max(target * CHUNK_ROWS) - target * CHUNK_ROWS;
    let hi = (end as usize).min((target + 1) * CHUNK_ROWS) - target * CHUNK_ROWS;
    Ok(scratch[lo..hi]
        .binary_search(&value)
        .ok()
        .map(|at| (target * CHUNK_ROWS + lo + at) as u64))
}

/// The trailer of one segment, held in memory so repeated sorted
/// lookups skip the span read a cold [`probe`] pays. Sixteen bytes a
/// chunk, so about 75 KiB for a 4.8 M value segment.
#[derive(Debug, Clone)]
pub struct ChunkDirectory {
    entries: Vec<Entry>,
}

impl ChunkDirectory {
    /// The fence of chunk `i`, its last value.
    fn fence(&self, i: usize) -> u64 {
        self.entries[i].aux
    }
}

impl crate::cache::PoolBytes for ChunkDirectory {
    fn pool_bytes(&self) -> usize {
        self.entries.len() * ENTRY_BYTES
    }
}

/// Loads the trailer of `meta`'s segment.
pub fn load_chunk_directory(db: &mut Zu1File, meta: &SegmentMeta) -> Result<ChunkDirectory> {
    if meta.structural != Structural::MiniBlock {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "MiniBlock reader given a FullZip segment".to_string(),
        });
    }
    Ok(ChunkDirectory {
        entries: read_trailer(db, meta)?,
    })
}

/// Loads `meta`'s chunk directory through `pool`, keyed like
/// [`read_segment_pooled`], so forked readers decode each directory
/// once between them instead of once per handle.
pub fn load_chunk_directory_pooled(
    db: &mut Zu1File,
    pool: &DecodedPool<ChunkDirectory>,
    meta: &SegmentMeta,
) -> Result<Arc<ChunkDirectory>> {
    let key = meta.blocks[0];
    if let Some(dir) = pool.get(key) {
        return Ok(dir);
    }
    let dir = Arc::new(load_chunk_directory(db, meta)?);
    pool.insert(key, Arc::clone(&dir));
    Ok(dir)
}

/// Decoded chunks held by a reader between lookups, one slot per chunk
/// filled on first touch and kept. Warm means touched before: a lookup
/// that lands on a held chunk costs a binary search, no block read and
/// no decode, which is the B2 budget. Memory is bounded by the decoded
/// segment, the same order as the group cache the graph reader keeps;
/// eviction is the buffer manager's job (docs/09, M3).
#[derive(Debug, Default)]
pub struct ChunkCache {
    chunks: Vec<Vec<u64>>,
}

/// Decodes chunk `target` of `meta`'s segment into `out`, which is
/// cleared first. The scan path drives this with one reusable scratch
/// vector, since a scan touches each chunk once and a per-chunk cache
/// would only hold memory it never reads again.
pub fn decode_chunk(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    dir: &ChunkDirectory,
    target: usize,
    out: &mut Vec<u64>,
) -> Result<()> {
    let chunks = meta.chunk_count();
    if dir.entries.len() != chunks || target >= chunks {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "chunk directory disagrees with meta".to_string(),
        });
    }
    decode_extent(db, meta, dir.entries[target], meta.chunk_rows(target), out)
}

/// The inclusive value bounds of chunk `i`, known without decoding when
/// the writer recorded the segment as sorted: the previous fence floors
/// the chunk and its own fence caps it. `None` when the segment is
/// unsorted and the fences are just last values, not bounds.
pub fn chunk_zone(meta: &SegmentMeta, dir: &ChunkDirectory, i: usize) -> Option<(u64, u64)> {
    if !meta.sorted || i >= dir.entries.len() {
        return None;
    }
    let lo = if i == 0 { meta.min } else { dir.fence(i - 1) };
    Some((lo, dir.fence(i)))
}

/// Decodes chunk `target` of `meta`'s segment through `cache`, reusing
/// the held values when the chunk was decoded before.
pub fn cached_chunk<'a>(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    dir: &ChunkDirectory,
    cache: &'a mut ChunkCache,
    target: usize,
) -> Result<&'a [u64]> {
    let chunks = meta.chunk_count();
    if target >= chunks {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "chunk directory disagrees with meta".to_string(),
        });
    }
    if cache.chunks.is_empty() {
        cache.chunks.resize(chunks, Vec::new());
    }
    if cache.chunks[target].is_empty() {
        let mut values = Vec::with_capacity(meta.chunk_rows(target));
        decode_chunk(db, meta, dir, target, &mut values)?;
        cache.chunks[target] = values;
    }
    Ok(&cache.chunks[target])
}

/// [`find_in_sorted`] through a decoded-chunk cache: a warm lookup that
/// lands on the cached chunk costs a fence partition point and a binary
/// search, no read and no decode.
pub fn find_in_sorted_cached(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    dir: &ChunkDirectory,
    cache: &mut ChunkCache,
    value: u64,
) -> Result<Option<u64>> {
    if meta.value_count == 0 || value < meta.min || value > meta.max {
        return Ok(None);
    }
    let target = dir.entries.partition_point(|e| e.aux < value);
    if target == meta.chunk_count() {
        return Ok(None);
    }
    let chunk = cached_chunk(db, meta, dir, cache, target)?;
    Ok(chunk
        .binary_search(&value)
        .ok()
        .map(|i| (target * CHUNK_ROWS + i) as u64))
}

/// Reads the single value at `pos` through a decoded-chunk cache, the
/// warm point companion of [`read_range`].
pub fn read_one_cached(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    dir: &ChunkDirectory,
    cache: &mut ChunkCache,
    pos: u64,
) -> Result<u64> {
    if pos >= meta.value_count {
        return Err(ZuError::InvalidArgument(format!(
            "position {pos} out of 0..{}",
            meta.value_count
        )));
    }
    let chunk = cached_chunk(db, meta, dir, cache, pos as usize / CHUNK_ROWS)?;
    Ok(chunk[pos as usize % CHUNK_ROWS])
}

/// Position of `value` in a segment whose rows are sorted ascending, or
/// `None` when absent. The fences name the single chunk that could hold
/// the value, so a hit costs one chunk decode; the zone map answers
/// misses outside `[min, max]` from the meta alone.
pub fn find_in_sorted(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    dir: &ChunkDirectory,
    value: u64,
) -> Result<Option<u64>> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if meta.structural != Structural::MiniBlock {
        return Err(corrupt("MiniBlock reader given a FullZip segment"));
    }
    if meta.value_count == 0 || value < meta.min || value > meta.max {
        return Ok(None);
    }
    let chunks = meta.chunk_count();
    if dir.entries.len() != chunks {
        return Err(corrupt("chunk directory disagrees with meta"));
    }
    let target = dir.entries.partition_point(|e| e.aux < value);
    if target == chunks {
        return Ok(None);
    }
    let mut scratch = Vec::with_capacity(CHUNK_ROWS);
    decode_extent(
        db,
        meta,
        dir.entries[target],
        meta.chunk_rows(target),
        &mut scratch,
    )?;
    Ok(scratch
        .binary_search(&value)
        .ok()
        .map(|i| (target * CHUNK_ROWS + i) as u64))
}

/// Reads payload bytes `[from, to)` through the block cache. A span
/// inside one block, the common case, borrows the pinned frame with no
/// copy; a span crossing blocks assembles an owned copy.
pub(crate) fn read_payload_span(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    from: usize,
    to: usize,
) -> Result<SegmentBytes> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    if from > to || to > meta.payload_len as usize {
        return Err(corrupt("span outside the payload"));
    }
    let block = BLOCK_SIZE as usize;
    let past_list = || corrupt("span past the block list");
    if to == from {
        return Ok(SegmentBytes::Owned(Vec::new()));
    }
    if from / block == (to - 1) / block {
        let ptr = *meta.blocks.get(from / block).ok_or_else(past_list)?;
        return Ok(SegmentBytes::Pinned {
            block: db.pin_block(ptr)?,
            start: from % block,
            len: to - from,
        });
    }
    let mut buf = Vec::with_capacity(to - from);
    let mut pos = from;
    while pos < to {
        let offset = pos % block;
        let len = (to - pos).min(block - offset);
        let ptr = *meta.blocks.get(pos / block).ok_or_else(past_list)?;
        buf.extend_from_slice(&db.pin_block(ptr)?[offset..offset + len]);
        pos += len;
    }
    Ok(SegmentBytes::Owned(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewrite_keeps_the_chunks_it_was_not_told_about() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let mut values: Vec<u64> = (0..10_000u64).map(|i| i * 7 % 811).collect();
        let first = write_segment(&mut db, &values).unwrap();

        // Nothing changed, so nothing is written at all.
        let same = rewrite_segment(&mut db, &first, &BTreeMap::new(), &[]).unwrap();
        assert_eq!(same, first);

        // One cell of one chunk, which is the shape of a point write.
        // Every block but the one the new chunk landed in comes across
        // by pointer, which is the whole of what this costs.
        values[2500] = 900_001;
        let after = rewrite_segment(
            &mut db,
            &first,
            &BTreeMap::from([(2500u64, 900_001u64)]),
            &[],
        )
        .unwrap();
        let mut out = Vec::new();
        read_segment(&mut db, &after, &mut out).unwrap();
        assert_eq!(out, values, "and the values read back are the new ones");
        assert_eq!(after.max, 900_001, "the zone map is the segment's");
        assert_eq!(
            after.blocks[..first.blocks.len() - 1],
            first.blocks[..first.blocks.len() - 1],
            "the blocks under the write are the same blocks"
        );

        // A column that grew: the last chunk was partial and is not
        // the same chunk any more, and the ones under it still are.
        let grew: Vec<u64> = (5_000..5_400u64).collect();
        let grown = rewrite_segment(&mut db, &after, &BTreeMap::new(), &grew).unwrap();
        values.extend(&grew);
        let mut out = Vec::new();
        read_segment(&mut db, &grown, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn an_append_writes_two_blocks_whatever_the_column_holds() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        // Random values so nothing packs and the column is 800 KB, four
        // blocks, against which one appended row is nothing.
        let mut rng = 0x5EEDu64;
        let values: Vec<u64> = (0..100_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            })
            .collect();
        let meta = write_segment(&mut db, &values).unwrap();
        assert!(meta.blocks.len() >= 4);
        let before = db.db_header().block_count;
        let grown = rewrite_segment(&mut db, &meta, &BTreeMap::new(), &[7]).unwrap();
        let took = db.db_header().block_count - before;
        assert!(took <= 2, "an append took {took} blocks");
        let mut out = Vec::new();
        read_segment(&mut db, &grown, &mut out).unwrap();
        assert_eq!(out.len(), values.len() + 1);
        assert_eq!(out[..values.len()], values);
    }

    #[test]
    fn a_segment_written_over_is_laid_out_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let mut values: Vec<u64> = (0..10_000u64).map(|i| i * 7 % 811).collect();
        let mut meta = write_segment(&mut db, &values).unwrap();
        // Write into every chunk in turn. Each rewrite leaves the old
        // chunk behind as a hole, so the body grows until the holes are
        // worth more than half of it and the segment is packed again.
        for round in 0..3 {
            for chunk in 0..values.len().div_ceil(CHUNK_ROWS) {
                let row = (chunk * CHUNK_ROWS) as u64;
                let value = 1_000_000 + round * 1000 + row;
                values[row as usize] = value;
                meta =
                    rewrite_segment(&mut db, &meta, &BTreeMap::from([(row, value)]), &[]).unwrap();
                let body = meta.body_len().unwrap() as u64;
                assert!(
                    (body - meta.live_bytes) * 2 <= body,
                    "garbage above half the body"
                );
            }
        }
        let mut out = Vec::new();
        read_segment(&mut db, &meta, &mut out).unwrap();
        assert_eq!(out, values);
    }

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
        // Put chunk 2's extent past the end of the body; the point path
        // skips the checksum, so it must fail on its own bounds checks.
        let at = meta.body_len().unwrap() + 4 + 2 * ENTRY_BYTES;
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        db.write_block(meta.blocks[0], &block).unwrap();
        let mut out = Vec::new();
        assert!(read_range(&mut db, &meta, 2048, 2050, &mut out).is_err());
        // A length that runs off the end from a start that does not.
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        block[at + 4..at + 8].copy_from_slice(&u32::MAX.to_le_bytes());
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
    fn find_in_sorted_matches_binary_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        // Sorted evens across several chunks so every odd is a miss with
        // present values on both sides.
        let values: Vec<u64> = (0..5000u64).map(|i| i * 2 + 10).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        let cd = load_chunk_directory(&mut db, &meta).unwrap();
        for v in [
            0u64,
            9,
            10,
            11,
            2056,
            2057,
            2058,
            5000,
            10_007,
            10_008,
            10_009,
            u64::MAX,
        ] {
            let want = values.binary_search(&v).ok().map(|i| i as u64);
            let got = find_in_sorted(&mut db, &meta, &cd, v).unwrap();
            assert_eq!(got, want, "value {v}");
        }
        // The empty segment finds nothing.
        let empty = write_segment(&mut db, &[]).unwrap();
        let cd = load_chunk_directory(&mut db, &empty).unwrap();
        assert_eq!(find_in_sorted(&mut db, &empty, &cd, 0).unwrap(), None);
    }

    #[test]
    fn corrupt_fence_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).map(|i| i * 3).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        // Flip a bit inside chunk 2's fence. The checksum catches it on
        // the full path, so rewrite the checksum too and rely on the
        // fence cross check instead, which is what protects the probe
        // path.
        let at = meta.body_len().unwrap() + 4 + 2 * ENTRY_BYTES + 8;
        let mut block = db.read_block(meta.blocks[0]).unwrap();
        block[at] ^= 0xFF;
        db.write_block(meta.blocks[0], &block).unwrap();
        let mut patched = meta.clone();
        patched.crcs[0] = crc32c::crc32c(&block[..patched.payload_len as usize]);
        let mut out = Vec::new();
        let err = read_segment(&mut db, &patched, &mut out).unwrap_err();
        assert!(format!("{err}").contains("fence"));
    }

    #[test]
    fn hostile_meta_block_count_rejected() {
        // A 48 byte header whose payload length and block count agree
        // with each other but not with the bytes present: the claimed
        // list must fail the size check before it sizes an allocation.
        let block_count = 0x1000_0000u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&100u64.to_le_bytes());
        bytes.extend_from_slice(&(u64::from(block_count) * u64::from(BLOCK_SIZE)).to_le_bytes());
        bytes.extend_from_slice(&800u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&99u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&block_count.to_le_bytes());
        bytes.push(0);
        let err = SegmentMeta::decode(&bytes, 0).unwrap_err();
        assert!(format!("{err}").contains("truncated block list"));
    }

    #[test]
    fn unknown_structural_id_rejected_by_name() {
        let meta = SegmentMeta {
            value_count: 10,
            payload_len: 80,
            uncompressed_bytes: 80,
            min: 0,
            max: 9,
            live_bytes: 80,
            structural: Structural::MiniBlock,
            sorted: false,
            blocks: vec![3],
            crcs: vec![0],
        };
        let mut bytes = Vec::new();
        meta.encode(&mut bytes);
        bytes[52] = 9;
        let err = SegmentMeta::decode(&bytes, 0).unwrap_err();
        assert!(format!("{err}").contains("structural layout"));
        assert!(format!("{err}").contains('9'));
    }

    #[test]
    fn miniblock_readers_reject_fullzip_metas() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).collect();
        let mut meta = write_segment(&mut db, &values).unwrap();
        meta.structural = Structural::FullZip;
        let mut out = Vec::new();
        assert!(read_segment(&mut db, &meta, &mut out).is_err());
        assert!(read_range(&mut db, &meta, 0, 10, &mut out).is_err());
        assert!(probe(&mut db, &meta, 0, 10, 5).is_err());
        assert!(load_chunk_directory(&mut db, &meta).is_err());
    }

    #[test]
    fn hostile_zone_min_above_max_rejected() {
        let meta = SegmentMeta {
            value_count: 10,
            payload_len: 80,
            uncompressed_bytes: 80,
            min: 5,
            max: 4,
            live_bytes: 80,
            structural: Structural::MiniBlock,
            sorted: false,
            blocks: vec![3],
            crcs: vec![0],
        };
        let mut bytes = Vec::new();
        meta.encode(&mut bytes);
        let err = SegmentMeta::decode(&bytes, 0).unwrap_err();
        assert!(format!("{err}").contains("zone min above max"));
    }

    #[test]
    fn corrupt_zone_is_rejected_by_full_scans() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).map(|i| i * 3).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        assert_eq!((meta.min, meta.max), (0, 4999 * 3));
        // The checksums cover the payload, not the meta, so a zone
        // that drifted from the values must fall to the cross check
        // alone.
        let mut patched = meta.clone();
        patched.max -= 1;
        let mut out = Vec::new();
        let err = read_segment(&mut db, &patched, &mut out).unwrap_err();
        assert!(format!("{err}").contains("zone"));
    }

    #[test]
    fn zone_prunes_probes_without_reading_the_payload() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let values: Vec<u64> = (0..5000u64).map(|i| 100 + i * 2).collect();
        let meta = write_segment(&mut db, &values).unwrap();
        // Destroy the payload wholesale. A probe outside the zone must
        // still answer absent because it never reads a byte of it, and
        // one inside the zone must fail on the wreckage, proving the
        // difference is the early out and not luck.
        for &ptr in &meta.blocks {
            db.write_block(ptr, &vec![0u8; BLOCK_SIZE as usize])
                .unwrap();
        }
        let end = values.len() as u64;
        assert!(!probe(&mut db, &meta, 0, end, 99).unwrap());
        assert!(!probe(&mut db, &meta, 0, end, 100 + 4999 * 2 + 1).unwrap());
        assert!(!probe(&mut db, &meta, 0, end, u64::MAX).unwrap());
        assert!(probe(&mut db, &meta, 0, end, 5000).is_err());
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
