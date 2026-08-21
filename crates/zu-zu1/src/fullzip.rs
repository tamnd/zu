//! Column segments in the FullZip structural layout of
//! `docs/04-storage-zu1-format.md` §3: variable-width values zipped with
//! their lengths, for STRING past the dict threshold, BLOB, VECTOR, and
//! LIST payloads. Zipping means a row range is one contiguous byte range,
//! and the per-chunk offset samples make the seek to any row O(1): jump
//! to the chunk holding it, then walk at most 1023 inline lengths.
//!
//! Payload layout, all little-endian: the body of concatenated chunks
//! first, then the trailer, which is `chunk_count: u32` and one entry
//! per chunk of `start: u32`, `len: u32`, `zipped_size: u64`. It is the
//! same trailer MiniBlock writes, entry for entry, with the fence slot
//! carrying the chunk's zipped size instead of its last value.
//!
//! Body before index, and an extent rather than a cumulative end, are
//! what let a chunk be written again without the segment being written
//! again: the new bytes go on the end of the body, the entry points at
//! them, and nothing that did not change moves. What the old bytes
//! leave behind is a hole, `live_bytes` counts what is still pointed
//! at, and a rewrite that finds the holes worth more than half the body
//! lays the segment out again without them.
//!
//! Each chunk covers up to 1024 rows and starts with one encoding id
//! byte: Plain (0) is the zipped form itself, one `len: u32` before each
//! row's bytes, and FSST (8) is that form behind `zu_encoding::fsst`,
//! self-contained symbol table and all. The writer keeps whichever is
//! smaller per chunk. The zipped sizes are what let a reader size and
//! verify every chunk before touching it: the FSST ceiling is exact, a
//! plain chunk must match to the byte, and a chunk claiming more than
//! [`MAX_CHUNK_RAW`] is rejected before anything is allocated.
//!
//! The meta's `uncompressed_bytes` holds the total value bytes (lengths
//! excluded), and the full scan cross-checks the decoded total against
//! it. The zone map does not apply to byte payloads, so FullZip metas
//! carry `min = max = 0`. As with MiniBlock, the checksum is per block
//! and the full scan verifies every block and every structural claim;
//! the point path skips the checksum by design and bounds every access
//! instead.

use std::collections::{BTreeMap, BTreeSet};

use zu_common::{Result, ZuError};
use zu_encoding::{EncodingId, fsst};

use crate::BLOCK_SIZE;
use crate::file::Zu1File;
use crate::segment::{
    CHUNK_ROWS, ENTRY_BYTES, Entry, SegmentMeta, Structural, append_trailer, entry_span,
    parse_trailer, read_entry_run, read_payload_span, read_payload_verified, store_payload,
};

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

/// Roughly how many bytes of a column an FSST table is trained on. The
/// encoder samples again inside this, in evenly spaced fragments, so the
/// figure only has to be enough of the column to be representative of it
/// and being generous costs one memcpy.
const TRAIN_SAMPLE: usize = 256 << 10;

/// How many places down a column the sample is drawn from. Enough of
/// them that a column whose tail looks nothing like its head is still
/// represented, few enough that each one is a long run.
const TRAIN_RUNS: usize = 16;

/// One symbol table for the whole column, trained on runs of consecutive
/// values drawn from evenly spaced places down it. Training is 96% of
/// what encoding a chunk costs, so training per chunk is what made
/// writing a string column slow: 2M urls took 23.3s to encode, of which
/// all but a second was 1953 tables. The sample is zipped the way a
/// chunk is, lengths and all, because that is what the table will be
/// asked to encode.
///
/// The runs are what keeps the sample the same column the chunks are.
/// Taking one value every so many instead reads a column with a period
/// in it as a different column whenever the two share a factor: a
/// version column cycling 0 through 9, sampled every second row, is a
/// column cycling 0, 2, 4, 6, 8, and a table trained on that costs four
/// times the codes on the real thing. Every chunk then measures badly
/// against the sample and buys a table of its own, which is the cost
/// this function exists to avoid, and it showed up as 100ms to rewrite
/// one 100k row column on a single cell write.
///
/// A column that fits in the budget is sampled whole, since runs over
/// one that small would overlap and train on the same bytes twice.
///
/// The table comes back with what it was worth on the sample, because
/// that is the yardstick for a chunk that does not fit it: a column of
/// two dialects trains on both and serves each of them worse than a
/// table of its own would, so a chunk that lands well off the column's
/// own ratio buys a table rather than shipping uncompressed.
fn column_table(values: &[&[u8]]) -> (fsst::Table, u64, u64) {
    let sample = column_sample(values);
    let table = fsst::Table::train(&sample);
    let (packed, raw) = fit(&table, &sample);
    (table, packed, raw)
}

/// What a table is worth on a sample of the column: the code stream
/// without the table, and the bytes it stands for. This is the yardstick
/// [`ChunkEncoder::push`] measures a chunk against.
fn fit(table: &fsst::Table, sample: &[u8]) -> (u64, u64) {
    let mut probe = Vec::new();
    let packed = table.encode(sample, &mut probe) - table.header_len();
    (packed as u64, sample.len().max(1) as u64)
}

/// The sample a column's table is trained on and measured against.
fn column_sample(values: &[&[u8]]) -> Vec<u8> {
    let total: usize = values.iter().map(|v| v.len() + 4).sum();
    let mut sample = Vec::with_capacity(total.min(TRAIN_SAMPLE + BLOCK_SIZE as usize));
    let mut push = |v: &[u8]| {
        sample.extend_from_slice(&(v.len() as u32).to_le_bytes());
        sample.extend_from_slice(v);
    };
    if total <= TRAIN_SAMPLE {
        for v in values {
            push(v);
        }
    } else {
        let run = TRAIN_SAMPLE.div_ceil(TRAIN_RUNS);
        for r in 0..TRAIN_RUNS {
            let start = values.len() * r / TRAIN_RUNS;
            let mut taken = 0usize;
            for v in &values[start..] {
                if taken >= run {
                    break;
                }
                taken += v.len() + 4;
                push(v);
            }
        }
    }
    sample
}

/// Encodes `values` into the FullZip payload, appending to `out`, and
/// returns the total value bytes. Public so the fuzz seeds can build a
/// payload without a file around it.
pub fn encode_payload(values: &[&[u8]], out: &mut Vec<u8>) -> Result<u64> {
    encode_payload_with(values, None, out)
}

/// The same, with the option of a table the caller already has, which is
/// what a rewrite of a column that is already on disk hands in.
fn encode_payload_with(
    values: &[&[u8]],
    table: Option<(fsst::Table, u64, u64)>,
    out: &mut Vec<u8>,
) -> Result<u64> {
    let chunk_count = values.len().div_ceil(CHUNK_ROWS);
    let mut enc = match table {
        Some((table, packed, raw)) => ChunkEncoder::reusing(table, values, (packed, raw)),
        None => ChunkEncoder::train(values),
    };
    let base = out.len();
    let mut entries = Vec::with_capacity(chunk_count);
    let mut value_bytes = 0u64;
    for chunk in values.chunks(CHUNK_ROWS) {
        let start = out.len() - base;
        let (raw, bytes) = enc.push(chunk, out)?;
        value_bytes += bytes;
        let len = out.len() - base - start;
        if start + len > u32::MAX as usize {
            return Err(ZuError::InvalidArgument(
                "segment body exceeds the u32 chunk index range".to_string(),
            ));
        }
        entries.push(Entry {
            start: start as u32,
            len: len as u32,
            aux: raw,
        });
    }
    append_trailer(out, &entries);
    Ok(value_bytes)
}

/// One column's chunk encoder: the table the whole column is measured
/// against, what it was worth on the sample it trained on, and the
/// buffers every chunk reuses.
struct ChunkEncoder {
    table: fsst::Table,
    fit_packed: u64,
    fit_raw: u64,
    zipped: Vec<u8>,
    packed: Vec<u8>,
    own: Vec<u8>,
}

impl ChunkEncoder {
    fn train(values: &[&[u8]]) -> Self {
        let (table, fit_packed, fit_raw) = column_table(values);
        Self::new(table, fit_packed, fit_raw)
    }

    /// The same encoder around a table the caller brought, measured
    /// against this column so the per chunk guard still works. Training
    /// is 95% of what writing a string column costs, so a rewrite that
    /// keeps the table the column already had writes for the price of
    /// encoding.
    ///
    /// The table is kept only if it still earns what it used to: `was`
    /// is what the column on disk encoded to over the bytes it stood
    /// for, and a table that now costs a quarter more than that is a
    /// table for a column this no longer is, so it trains. The per chunk
    /// guard cannot make this call, because it measures a chunk against
    /// the column and a stale table makes both look the same.
    fn reusing(table: fsst::Table, values: &[&[u8]], was: (u64, u64)) -> Self {
        let sample = column_sample(values);
        let (fit_packed, fit_raw) = fit(&table, &sample);
        let (old_packed, old_raw) = was;
        if fit_packed * old_raw * 4 <= old_packed * fit_raw * 5 {
            return Self::new(table, fit_packed, fit_raw);
        }
        let trained = fsst::Table::train(&sample);
        let (fit_packed, fit_raw) = fit(&trained, &sample);
        Self::new(trained, fit_packed, fit_raw)
    }

    fn new(table: fsst::Table, fit_packed: u64, fit_raw: u64) -> Self {
        Self {
            table,
            fit_packed,
            fit_raw,
            zipped: Vec::new(),
            packed: Vec::new(),
            own: Vec::new(),
        }
    }

    /// Zips one chunk, keeps whichever of plain and FSST is smaller and
    /// appends it to `body`. Returns the chunk's zipped size and the
    /// value bytes in it.
    fn push(&mut self, chunk: &[&[u8]], body: &mut Vec<u8>) -> Result<(u64, u64)> {
        self.zipped.clear();
        let mut value_bytes = 0u64;
        for v in chunk {
            if self.zipped.len() + 4 + v.len() > MAX_CHUNK_RAW {
                return Err(ZuError::InvalidArgument(format!(
                    "fullzip chunk exceeds the {MAX_CHUNK_RAW} byte raw cap"
                )));
            }
            self.zipped
                .extend_from_slice(&(v.len() as u32).to_le_bytes());
            self.zipped.extend_from_slice(v);
            value_bytes += v.len() as u64;
        }
        self.packed.clear();
        let mut packed_len = self.table.encode(&self.zipped, &mut self.packed);
        // Half again the bytes the column's own sample cost is the line
        // between a chunk of the column and a chunk of something else,
        // and only the second kind pays to be trained for. It is a loose
        // line on purpose: a chunk that would gain a few percent from its
        // own table is not worth the 3.4ms, and one that would halve is.
        // Both sides are the code stream without the table, since a table
        // is a seventh of a chunk this size and a fraction of a percent
        // of the sample, and comparing the two with it in would retrain
        // every chunk of every column.
        let codes = (packed_len - self.table.header_len()) as u64;
        if codes * 2 * self.fit_raw > self.fit_packed * 3 * self.zipped.len() as u64 {
            self.own.clear();
            let own_len = fsst::Table::train(&self.zipped).encode(&self.zipped, &mut self.own);
            if own_len < packed_len {
                std::mem::swap(&mut self.packed, &mut self.own);
                packed_len = own_len;
            }
        }
        if packed_len < self.zipped.len() {
            body.push(EncodingId::Fsst as u8);
            body.extend_from_slice(&self.packed);
        } else {
            body.push(EncodingId::Plain as u8);
            body.extend_from_slice(&self.zipped);
        }
        Ok((self.zipped.len() as u64, value_bytes))
    }
}

/// Writes a whole payload across freshly allocated blocks.
fn store(
    db: &mut Zu1File,
    payload: &[u8],
    live: u64,
    value_count: u64,
    value_bytes: u64,
) -> Result<SegmentMeta> {
    let (blocks, crcs, payload_len) = store_payload(db, &[], &[], payload)?;
    Ok(SegmentMeta {
        value_count,
        payload_len,
        uncompressed_bytes: value_bytes,
        min: 0,
        max: 0,
        live_bytes: live,
        structural: Structural::FullZip,
        sorted: false,
        blocks,
        crcs,
    })
}

/// Encodes `values` chunk by chunk and writes the FullZip payload across
/// freshly allocated blocks.
pub fn write_blob_segment(db: &mut Zu1File, values: &[&[u8]]) -> Result<SegmentMeta> {
    let mut payload = Vec::new();
    let value_bytes = encode_payload(values, &mut payload)?;
    let live = live_of(&payload, values.len().div_ceil(CHUNK_ROWS))?;
    store(db, &payload, live, values.len() as u64, value_bytes)
}

/// The live bytes of a payload that was just laid out contiguously,
/// which is its body, read back off the trailer so there is one place
/// that knows how the two relate.
fn live_of(payload: &[u8], chunks: usize) -> Result<u64> {
    let body_len = payload
        .len()
        .checked_sub(4 + chunks * ENTRY_BYTES)
        .ok_or_else(|| corrupt("payload shorter than its trailer"))?;
    Ok(body_len as u64)
}

/// The same, for a column that is already on disk in some other order:
/// the values are all of them again, but the table they were encoded
/// with is still good, so it comes off the old segment instead of being
/// trained a second time on the same strings.
///
/// This is the shape an edge write leaves. A rel table holds its edges
/// sorted by the row they leave, so an edge added in the middle moves
/// every edge behind it, and the fold hands the whole column back in the
/// new order. Nothing about the strings changed, only where they sit, so
/// training on them again is work with a known answer: reading the table
/// back and encoding with it writes a column of the same size for the
/// cost of the encode, and on the LinkBench shape that is 20 ms of an
/// add-link statement gone.
///
/// A column that really is different from the one on disk is caught the
/// way it always was, per chunk: a chunk that encodes well off the
/// table's own ratio buys a table of its own. If the old segment holds
/// nothing to read a table out of, this trains one.
pub fn rewrite_blob_reordered(
    db: &mut Zu1File,
    old: &SegmentMeta,
    values: &[&[u8]],
) -> Result<SegmentMeta> {
    let table = table_of(db, old).map(|t| (t, old.payload_len, old.uncompressed_bytes.max(1)));
    let mut payload = Vec::new();
    let value_bytes = encode_payload_with(values, table, &mut payload)?;
    let live = live_of(&payload, values.len().div_ceil(CHUNK_ROWS))?;
    // Every row moved, so nothing is kept and nothing is freed here
    // either: the caller owns the old segment's blocks, which is the
    // one place this differs from [`rewrite_blob_segment`].
    store(db, &payload, live, values.len() as u64, value_bytes)
}

/// The symbol table of the old segment's first FSST chunk, or None when
/// the segment holds no chunk encoded with one. A payload that will not
/// parse is not an error here: the caller trains instead, and every
/// reader of it still fails on it the way it did before.
fn table_of(db: &mut Zu1File, old: &SegmentMeta) -> Option<fsst::Table> {
    let body_len = old.body_len().ok()?;
    let entries = read_entry_run(db, old, 0, old.chunk_count().checked_sub(1)?).ok()?;
    for e in entries {
        let span = entry_span(e, body_len).ok()?;
        let chunk = read_payload_span(db, old, span.start, span.end).ok()?;
        match chunk.first() {
            Some(&id) if id == EncodingId::Fsst as u8 => {
                return fsst::Table::read(&chunk[1..]).ok();
            }
            _ => continue,
        }
    }
    None
}

/// Writes the segment a fold leaves behind when it changed `updates`
/// and appended `appended` to `old`: the chunks holding a changed row
/// are read and encoded again, and every other chunk keeps the bytes it
/// already encodes to, index entries and all.
///
/// This is [`crate::segment::rewrite_segment`] for the byte side, and it
/// saves more than that one does, because a blob chunk costs a symbol
/// table to encode and a walk of its inline lengths to read. A one cell
/// write into a 100k row string column used to decode the column into a
/// vector per row and encode all 98 chunks of it back; now it touches
/// the one chunk the row is in. The table is trained on the rows that
/// are about to be written rather than on the column, which is exactly
/// what the rewrite exists to avoid reading, and a chunk carries its own
/// table when it needs one, so a chunk encoded now still decodes beside
/// chunks encoded against an older table.
pub fn rewrite_blob_segment(
    db: &mut Zu1File,
    old: &SegmentMeta,
    updates: &BTreeMap<u64, Vec<u8>>,
    appended: &[Vec<u8>],
) -> Result<SegmentMeta> {
    if old.structural != Structural::FullZip {
        return Err(corrupt("FullZip rewrite given a MiniBlock segment"));
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
    let chunk_count = (new_count as usize).div_ceil(CHUNK_ROWS);
    let body_len = old.body_len()?;
    let mut entries = match old_chunks {
        0 => Vec::new(),
        n => read_entry_run(db, old, 0, n - 1)?,
    };
    for &e in &entries {
        entry_span(e, body_len)?;
    }
    entries.resize(chunk_count, Entry::default());

    // A chunk is rewritten when something wrote into it, when it is
    // past the old end, or when it no longer holds the rows it used to,
    // since short of the row count nothing about a chunk is the same.
    let rows_in = |i: usize| ((i + 1) * CHUNK_ROWS).min(new_count as usize) - i * CHUNK_ROWS;
    let mut dirty: BTreeSet<usize> = updates
        .keys()
        .map(|&row| row as usize / CHUNK_ROWS)
        .collect();
    dirty.extend(old_chunks..chunk_count);
    if old_chunks > 0 && old.chunk_rows(old_chunks - 1) != rows_in(old_chunks - 1) {
        dirty.insert(old_chunks - 1);
    }

    // The rows of the chunks that have to be encoded again, gathered
    // first so the table trains on them. The copy per row is what the
    // old path paid for the whole column and this one pays for the
    // chunks a statement wrote into.
    let mut rebuilt: Vec<(usize, Vec<Vec<u8>>)> = Vec::with_capacity(dirty.len());
    for &i in &dirty {
        let lo = i * CHUNK_ROWS;
        let hi = lo + rows_in(i);
        let mut rows: Vec<Vec<u8>> = Vec::with_capacity(hi - lo);
        let held = hi.min(base as usize);
        if lo < held {
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            read_blob_range(db, old, lo as u64, held as u64, &mut bytes, &mut ends)?;
            let mut start = 0usize;
            for &end in &ends {
                rows.push(bytes[start..end as usize].to_vec());
                start = end as usize;
            }
        }
        for row in lo.max(held)..hi {
            rows.push(appended[row - base as usize].clone());
        }
        for (&row, value) in updates.range(lo as u64..hi as u64) {
            rows[row as usize - lo] = value.clone();
        }
        rebuilt.push((i, rows));
    }

    // What the kept chunks are worth is inherited rather than
    // recomputed, because recomputing it means reading them. The value
    // bytes of a chunk are its zipped size less the `len: u32` in front
    // of each of its rows, and the zipped size is in the entry.
    let mut value_bytes = old.uncompressed_bytes;
    let mut live = old.live_bytes;
    for &i in &dirty {
        if i < old_chunks {
            value_bytes = value_bytes
                .checked_sub(
                    entries[i]
                        .aux
                        .checked_sub(4 * old.chunk_rows(i) as u64)
                        .ok_or_else(|| corrupt("chunk shorter than its row lengths"))?,
                )
                .ok_or_else(|| corrupt("chunk value bytes above the column's"))?;
            live -= u64::from(entries[i].len);
        }
    }

    let sample: Vec<&[u8]> = rebuilt
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|v| v.as_slice()))
        .collect();
    let mut enc = ChunkEncoder::train(&sample);
    let mut fresh = Vec::new();
    for (i, rows) in &rebuilt {
        let refs: Vec<&[u8]> = rows.iter().map(|v| v.as_slice()).collect();
        let start = body_len + fresh.len();
        let (raw, bytes) = enc.push(&refs, &mut fresh)?;
        let len = body_len + fresh.len() - start;
        if start + len > u32::MAX as usize {
            return Err(ZuError::InvalidArgument(
                "segment body exceeds the u32 chunk index range".to_string(),
            ));
        }
        entries[*i] = Entry {
            start: start as u32,
            len: len as u32,
            aux: raw,
        };
        live += len as u64;
        value_bytes += bytes;
    }

    let new_body_len = body_len + fresh.len();
    let head = SegmentMeta {
        value_count: new_count,
        payload_len: 0,
        uncompressed_bytes: value_bytes,
        min: 0,
        max: 0,
        live_bytes: live,
        structural: Structural::FullZip,
        sorted: false,
        blocks: Vec::new(),
        crcs: Vec::new(),
    };
    if (new_body_len as u64 - live) * 2 > new_body_len as u64 {
        return compact_blob_segment(db, old, head, &entries, &fresh, body_len);
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

/// Lays a blob segment out again in row order with the holes taken out,
/// the FullZip half of [`crate::segment::rewrite_segment`]'s compaction
/// and on the same terms: a chunk's bytes already encode exactly its
/// rows, so this copies and nothing is decoded or zipped again.
fn compact_blob_segment(
    db: &mut Zu1File,
    old: &SegmentMeta,
    head: SegmentMeta,
    entries: &[Entry],
    fresh: &[u8],
    body_len: usize,
) -> Result<SegmentMeta> {
    let payload = read_payload_verified(db, old)?;
    let body = payload
        .get(..body_len)
        .ok_or_else(|| corrupt("payload shorter than its body"))?;
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
                    .ok_or_else(|| corrupt("chunk extent outside the body"))?;
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

/// One chunk's zipped bytes: borrowed straight from a plain chunk,
/// FSST-decoded into `scratch` otherwise. `raw_size` comes from the
/// index and every path checks it against [`MAX_CHUNK_RAW`] first, so it
/// is both the exact decode ceiling and the verified result size.
fn unpack_chunk<'a>(
    chunk: &'a [u8],
    raw_size: usize,
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    let (&enc, payload) = chunk.split_first().ok_or_else(|| corrupt("empty chunk"))?;
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
        let body_len = payload
            .len()
            .checked_sub(4 + chunks * ENTRY_BYTES)
            .ok_or_else(|| corrupt("payload shorter than its trailer"))?;
        let entries = parse_trailer(&payload[body_len..], chunks)?;
        let body = &payload[..body_len];
        let mut scratch = Vec::new();
        for (i, &e) in entries.iter().enumerate() {
            let raw_size = e.aux;
            if raw_size > MAX_CHUNK_RAW as u64 {
                return Err(corrupt("chunk above the raw cap"));
            }
            let rows = (value_count as usize - i * CHUNK_ROWS).min(CHUNK_ROWS);
            let zipped = unpack_chunk(
                &body[entry_span(e, body_len)?],
                raw_size as usize,
                &mut scratch,
            )?;
            unzip_rows(zipped, rows, 0, rows, true, bytes_out, ends_out)?;
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
    let payload = read_payload(db, meta)?;
    decode_payload(
        &payload,
        meta.value_count,
        meta.uncompressed_bytes,
        bytes_out,
        ends_out,
    )
}

/// The whole payload of a segment, block by block, each one verified
/// against its own checksum on the way past.
fn read_payload(db: &mut Zu1File, meta: &SegmentMeta) -> Result<Vec<u8>> {
    if meta.structural != Structural::FullZip {
        return Err(corrupt("FullZip reader given a MiniBlock segment"));
    }
    read_payload_verified(db, meta)
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
        let body_len = meta.body_len()?;
        let first = start as usize / CHUNK_ROWS;
        let last = (end - 1) as usize / CHUNK_ROWS;
        // One span read for the entries of the covered chunks, then one
        // per chunk for its bytes, since a chunk a rewrite moved sits at
        // the tail rather than beside its neighbours.
        let entries = read_entry_run(db, meta, first, last)?;
        let mut scratch = Vec::new();
        for i in first..=last {
            let e = entries[i - first];
            let raw_size = e.aux;
            if raw_size > MAX_CHUNK_RAW as u64 {
                return Err(corrupt("chunk above the raw cap"));
            }
            let span = entry_span(e, body_len)?;
            let bytes = read_payload_span(db, meta, span.start, span.end)?;
            let rows = meta.chunk_rows(i);
            let lo_row = (start as usize).max(i * CHUNK_ROWS) - i * CHUNK_ROWS;
            let hi_row = (end as usize).min((i + 1) * CHUNK_ROWS) - i * CHUNK_ROWS;
            let zipped = unpack_chunk(&bytes, raw_size as usize, &mut scratch)?;
            unzip_rows(zipped, rows, lo_row, hi_row, false, bytes_out, ends_out)?;
        }
        Ok(())
    };
    run(db, bytes_out, ends_out).inspect_err(|_| {
        bytes_out.truncate(base_bytes);
        ends_out.truncate(base_ends);
    })
}

/// Point access to one row: appends value `row`'s bytes to `out`.
pub fn read_blob_row(
    db: &mut Zu1File,
    meta: &SegmentMeta,
    row: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
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
                format!("https://example.com/user/{}/posts?page={}", i * 37, i % 12).into_bytes()
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

    /// A column with a period in it trains a table that fits its own
    /// chunks, so no chunk buys one of its own. Taking one value every
    /// so many read this column as a different column: a version column
    /// cycling 0 through 9 sampled every second row is a column cycling
    /// 0, 2, 4, 6, 8, the table that came off it cost four times the
    /// codes on the real chunks, all 98 of them trained their own, and
    /// rewriting the column on a one cell write took 100ms.
    ///
    /// The assertion is the encoder's own retrain test, run against the
    /// first chunk, because that is the thing that must not fire.
    #[test]
    fn a_periodic_column_trains_on_itself() {
        let values: Vec<Vec<u8>> = (0..100_000u32)
            .map(|i| (i % 10).to_string().into_bytes())
            .collect();
        let values = refs(&values);
        let (table, fit_packed, fit_raw) = column_table(&values);
        let mut zipped = Vec::new();
        for v in &values[..CHUNK_ROWS] {
            zipped.extend_from_slice(&(v.len() as u32).to_le_bytes());
            zipped.extend_from_slice(v);
        }
        let mut packed = Vec::new();
        let codes = (table.encode(&zipped, &mut packed) - table.header_len()) as u64;
        assert!(
            codes * 2 * fit_raw <= fit_packed * 3 * zipped.len() as u64,
            "chunk packs {codes} from {}, sample packed {fit_packed} from {fit_raw}",
            zipped.len()
        );
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
    fn one_table_serves_a_column_whose_halves_differ() {
        // The column's table is trained once on a stride across the whole
        // column, so a column that changes shape halfway is the case that
        // would suffer from it. Both halves must read back, and the
        // second half must still compress: a table that only saw urls
        // would leave those rows escaping byte by byte.
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let mut values = urls(5000);
        values
            .extend((0..5000usize).map(|i| {
                format!("/var/log/service-{}/rotated.{}.log", i % 40, i % 7).into_bytes()
            }));
        let meta = write_blob_segment(&mut db, &refs(&values)).unwrap();
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_blob_segment(&mut db, &meta, &mut bytes, &mut ends).unwrap();
        for (i, v) in values.iter().enumerate() {
            assert_eq!(value(&bytes, &ends, i), v.as_slice(), "row {i}");
        }
        let head = write_blob_segment(&mut db, &refs(&values[..5000])).unwrap();
        let tail = write_blob_segment(&mut db, &refs(&values[5000..])).unwrap();
        let apart = head.payload_len + tail.payload_len;
        // Within a tenth of a column each, which is what the retrain
        // buys: without it the mixed column is half again the size,
        // because a table trained on both dialects serves neither.
        assert!(
            meta.payload_len * 10 < apart * 11,
            "one table for both halves cost more than a tenth over a table each, {} against {apart}",
            meta.payload_len
        );
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
        assert!(read_blob_range(&mut db, &meta, 0, 5001, &mut out, &mut ends).is_err());
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

    /// Every value of a segment, as the readers hand them back.
    fn all(db: &mut Zu1File, meta: &SegmentMeta) -> Vec<Vec<u8>> {
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        read_blob_segment(db, meta, &mut bytes, &mut ends).unwrap();
        (0..ends.len())
            .map(|i| value(&bytes, &ends, i).to_vec())
            .collect()
    }

    /// The compressed size of each chunk of a segment, which is what a
    /// copied chunk keeps and a re-encoded one is free to change.
    fn spans(db: &mut Zu1File, meta: &SegmentMeta) -> Vec<u64> {
        match meta.chunk_count() {
            0 => Vec::new(),
            n => read_entry_run(db, meta, 0, n - 1)
                .unwrap()
                .iter()
                .map(|e| u64::from(e.len))
                .collect(),
        }
    }

    /// A rewritten segment holds what a full write of the same rows
    /// would, whatever the write did: change a row, empty one, grow the
    /// last chunk, add chunks past it, or all of that at once.
    #[test]
    fn a_rewrite_holds_what_a_full_write_would() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        // 4500 rows, so the last chunk is a short one and appends have
        // to extend it before they open a new one.
        let base = urls(4500);
        let meta = write_blob_segment(&mut db, &refs(&base)).unwrap();
        let cases: Vec<(Vec<u64>, usize)> = vec![
            (vec![], 0),
            (vec![0], 0),
            (vec![4499], 0),
            (vec![0, 1023, 1024, 2047, 4499], 0),
            (vec![], 7),
            (vec![17], 700),
            (vec![1, 4400], 1500),
        ];
        for (rows, grow) in cases {
            let updates: BTreeMap<u64, Vec<u8>> = rows
                .iter()
                .enumerate()
                .map(|(k, &row)| {
                    let v = match k % 3 {
                        0 => format!("rewritten row {row}").into_bytes(),
                        1 => Vec::new(),
                        _ => vec![b'x'; 3000],
                    };
                    (row, v)
                })
                .collect();
            let appended: Vec<Vec<u8>> = (0..grow)
                .map(|i| format!("https://example.com/added/{i}").into_bytes())
                .collect();
            let mut want = base.clone();
            for (&row, v) in &updates {
                want[row as usize] = v.clone();
            }
            want.extend(appended.iter().cloned());

            let got = rewrite_blob_segment(&mut db, &meta, &updates, &appended).unwrap();
            let fresh = write_blob_segment(&mut db, &refs(&want)).unwrap();
            let case = format!("{rows:?} +{grow}");
            assert_eq!(got.value_count, want.len() as u64, "{case}");
            assert_eq!(got.uncompressed_bytes, fresh.uncompressed_bytes, "{case}");
            assert_eq!(all(&mut db, &got), want, "{case}");
        }
    }

    /// A column handed back in a different order reads back in that
    /// order, and reusing the table it already had costs it nothing on
    /// disk: it is the same strings, so the table that fit them still
    /// fits them.
    #[test]
    fn a_reordered_rewrite_keeps_the_values_and_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let base = urls(4500);
        let meta = write_blob_segment(&mut db, &refs(&base)).unwrap();
        // An edge write moves everything behind where it landed, so the
        // new order is the old one with a value pushed into the middle.
        let mut want = base.clone();
        want.insert(2000, b"https://example.com/user/added/1".to_vec());
        let got = rewrite_blob_reordered(&mut db, &meta, &refs(&want)).unwrap();
        assert_eq!(all(&mut db, &got), want);
        let fresh = write_blob_segment(&mut db, &refs(&want)).unwrap();
        assert_eq!(got.value_count, fresh.value_count);
        assert_eq!(got.uncompressed_bytes, fresh.uncompressed_bytes);
        assert!(
            got.payload_len <= fresh.payload_len * 21 / 20,
            "reused table wrote {} against {} trained",
            got.payload_len,
            fresh.payload_len
        );
    }

    /// A column that turned into a different column is not one the old
    /// table fits, and the per chunk guard is what catches that: every
    /// chunk buys a table of its own and the result is still near what
    /// training the column would have written.
    #[test]
    fn a_reordered_rewrite_of_other_strings_still_compresses() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let meta = write_blob_segment(&mut db, &refs(&urls(4500))).unwrap();
        let other: Vec<Vec<u8>> = (0..4500)
            .map(|i| format!("PREFIX-{i:06}-SUFFIX-{}", i % 7).into_bytes())
            .collect();
        let got = rewrite_blob_reordered(&mut db, &meta, &refs(&other)).unwrap();
        assert_eq!(all(&mut db, &got), other);
        let fresh = write_blob_segment(&mut db, &refs(&other)).unwrap();
        assert!(
            got.payload_len <= fresh.payload_len * 6 / 5,
            "reused table wrote {} against {} trained",
            got.payload_len,
            fresh.payload_len
        );
    }

    /// The point of the rewrite: a chunk no write named keeps the bytes
    /// it already encodes to, so a one cell change into a 4500 row
    /// column re-encodes one chunk of the five and copies the rest.
    #[test]
    fn a_rewrite_copies_the_chunks_no_write_named() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let base = urls(4500);
        let meta = write_blob_segment(&mut db, &refs(&base)).unwrap();
        let before = spans(&mut db, &meta);
        let updates =
            BTreeMap::from([(2000u64, b"https://example.com/user/1/posts?page=1".to_vec())]);
        let got = rewrite_blob_segment(&mut db, &meta, &updates, &[]).unwrap();
        let after = spans(&mut db, &got);
        assert_eq!(after.len(), before.len());
        for i in 0..before.len() {
            match i == 1 {
                true => continue,
                false => assert_eq!(after[i], before[i], "chunk {i} was re-encoded"),
            }
        }
    }

    /// Growing an empty column is the first fold of every new column,
    /// and it has no old chunk to copy from.
    #[test]
    fn a_rewrite_grows_an_empty_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let meta = write_blob_segment(&mut db, &[]).unwrap();
        let appended = urls(2500);
        let got = rewrite_blob_segment(&mut db, &meta, &BTreeMap::new(), &appended).unwrap();
        assert_eq!(all(&mut db, &got), appended);
    }

    #[test]
    fn a_rewrite_refuses_an_update_past_the_rows_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fz.zu1")).unwrap();
        let base = urls(100);
        let meta = write_blob_segment(&mut db, &refs(&base)).unwrap();
        let updates = BTreeMap::from([(100u64, b"past the end".to_vec())]);
        assert!(rewrite_blob_segment(&mut db, &meta, &updates, &[]).is_err());
        let mut fake = meta.clone();
        fake.structural = Structural::MiniBlock;
        assert!(rewrite_blob_segment(&mut db, &fake, &BTreeMap::new(), &[]).is_err());
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
        let mut payload = vec![EncodingId::Plain as u8, 0];
        append_trailer(
            &mut payload,
            &[Entry {
                start: 0,
                len: 2,
                aux: u64::from(u32::MAX) * 8,
            }],
        );
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        let err = decode_payload(&payload, 1, 10, &mut bytes, &mut ends).unwrap_err();
        assert!(format!("{err}").contains("raw cap"));
        // An extent that runs off the end of the body. Chunk 0 is a
        // legitimate chunk of 1024 empty values so the walk reaches the
        // bad entry, and the rejection must roll its rows back.
        let mut payload = vec![EncodingId::Plain as u8];
        payload.extend_from_slice(&[0u8; 4096]);
        append_trailer(
            &mut payload,
            &[
                Entry {
                    start: 0,
                    len: 4097,
                    aux: 4096,
                },
                Entry {
                    start: 4097,
                    len: 100,
                    aux: 4,
                },
            ],
        );
        let err = decode_payload(&payload, 1025, 0, &mut bytes, &mut ends).unwrap_err();
        assert!(format!("{err}").contains("outside the body"));
        assert!(bytes.is_empty() && ends.is_empty());
    }

    #[test]
    fn unknown_chunk_encoding_rejected_by_name() {
        let values: Vec<&[u8]> = vec![b"ab", b"cd"];
        let mut payload = Vec::new();
        encode_payload(&values, &mut payload).unwrap();
        // The body comes first, so the chunk's encoding byte is the
        // first byte of the payload.
        payload[0] = 200;
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        let err = decode_payload(&payload, 2, 4, &mut bytes, &mut ends).unwrap_err();
        assert!(format!("{err}").contains("200"));
    }
}
