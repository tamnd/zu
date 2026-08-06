//! Column segments: one encoded value stream stored across whole blocks.
//!
//! The payload is `zu_encoding::segment::encode_auto` output, so the
//! encoding id travels inside the payload and the reader needs no side
//! channel beyond the `SegmentMeta`. Metas serialize into meta-block
//! chains with a fixed little-endian layout: `value_count: u64`,
//! `payload_len: u64`, `uncompressed_bytes: u64`, `crc32c: u32`,
//! `block_count: u32`, then one `u64` per block pointer.

use zu_common::{Result, ZuError};
use zu_encoding::segment as enc;

use crate::BLOCK_SIZE;
use crate::file::{BlockPtr, Zu1File};

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
}

/// Encodes `values` with the cascade selector and writes the payload
/// across freshly allocated blocks.
pub fn write_segment(db: &mut Zu1File, values: &[u64]) -> Result<SegmentMeta> {
    let mut payload = Vec::new();
    enc::encode_auto(values, &mut payload);
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
    let mut payload = Vec::with_capacity(meta.payload_len as usize);
    for &ptr in &meta.blocks {
        let block = db.read_block(ptr)?;
        let want = (meta.payload_len as usize - payload.len()).min(block.len());
        payload.extend_from_slice(&block[..want]);
    }
    if payload.len() != meta.payload_len as usize {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "payload shorter than meta claims".to_string(),
        });
    }
    if crc32c::crc32c(&payload) != meta.crc {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "payload crc mismatch".to_string(),
        });
    }
    let start = out.len();
    enc::decode_any(&payload, out)?;
    if (out.len() - start) as u64 != meta.value_count {
        return Err(ZuError::Corrupt {
            what: "segment",
            detail: "decoded count disagrees with meta".to_string(),
        });
    }
    Ok(())
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
    fn empty_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("seg.zu1")).unwrap();
        let meta = write_segment(&mut db, &[]).unwrap();
        assert_eq!(meta.blocks.len(), 1, "even empty carries the id byte");
        let mut out = Vec::new();
        read_segment(&mut db, &meta, &mut out).unwrap();
        assert!(out.is_empty());
    }
}
