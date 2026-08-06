//! Manifest snapshot encoding.
//!
//! Hand-rolled binary, little-endian, versioned.
//! Layout: magic `ZUS3`, `format_version: u16`, `epoch: u64`, `writer_id: u128`, `segment_count: u32`, per segment a `key_len: u16` plus UTF-8 key bytes, then `crc32c: u32` over everything before it.
//! Decoding never panics: truncation, bad magic, and bad checksum all return `ZuError::Corrupt`, and a newer `format_version` returns `ZuError::Unsupported`.

use zu_common::{Result, ZuError};

const MAGIC: [u8; 4] = *b"ZUS3";
const FORMAT_VERSION: u16 = 1;

/// Byte offset of the segment directory: magic + version + epoch + writer_id + count.
const DIR_START: usize = 4 + 2 + 8 + 16 + 4;

/// Smallest well-formed manifest: an empty directory plus the trailing crc.
const MIN_LEN: usize = DIR_START + 4;

/// One immutable manifest snapshot, the root of a database state.
///
/// `epoch` increments by exactly one per commit and `writer_id` names the
/// single writer allowed to produce the next epoch (`docs/06-storage-s3.md`).
/// `segments` holds the object keys of the segment packs in this snapshot;
/// richer per-segment metadata (byte ranges, footers) lands with pack support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub epoch: u64,
    pub writer_id: u128,
    pub segments: Vec<String>,
}

impl Manifest {
    /// Encodes the manifest into the on-disk byte layout.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let count = u32::try_from(self.segments.len()).map_err(|_| {
            ZuError::InvalidArgument("manifest holds more than u32::MAX segment keys".to_string())
        })?;
        let dir_bytes: usize = self.segments.iter().map(|k| 2 + k.len()).sum();
        let mut out = Vec::with_capacity(MIN_LEN + dir_bytes);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.writer_id.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        for key in &self.segments {
            let len = u16::try_from(key.len()).map_err(|_| {
                ZuError::InvalidArgument(format!("segment key exceeds {} bytes: {key}", u16::MAX))
            })?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(key.as_bytes());
        }
        out.extend_from_slice(&crc32c::crc32c(&out).to_le_bytes());
        Ok(out)
    }

    /// Decodes a manifest, verifying magic, format version, and checksum.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "manifest",
            detail: detail.to_string(),
        };
        if bytes.len() < MIN_LEN {
            return Err(corrupt("truncated header"));
        }
        if bytes[..4] != MAGIC {
            return Err(corrupt("bad magic"));
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(ZuError::Unsupported {
                what: "manifest format",
                id: u32::from(version),
            });
        }
        let body_end = bytes.len() - 4;
        let stored = u32::from_le_bytes(bytes[body_end..].try_into().unwrap());
        if crc32c::crc32c(&bytes[..body_end]) != stored {
            return Err(corrupt("crc mismatch"));
        }
        let epoch = u64::from_le_bytes(bytes[6..14].try_into().unwrap());
        let writer_id = u128::from_le_bytes(bytes[14..30].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[30..DIR_START].try_into().unwrap());
        let body = &bytes[..body_end];
        let mut segments = Vec::new();
        let mut pos = DIR_START;
        for _ in 0..count {
            let len_bytes = body
                .get(pos..pos + 2)
                .ok_or_else(|| corrupt("truncated segment length"))?;
            let key_len = usize::from(u16::from_le_bytes(len_bytes.try_into().unwrap()));
            pos += 2;
            let key = body
                .get(pos..pos + key_len)
                .ok_or_else(|| corrupt("truncated segment key"))?;
            let key = std::str::from_utf8(key).map_err(|_| corrupt("segment key not utf-8"))?;
            segments.push(key.to_string());
            pos += key_len;
        }
        if pos != body_end {
            return Err(corrupt("trailing bytes"));
        }
        Ok(Self {
            epoch,
            writer_id,
            segments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            epoch: 7,
            writer_id: 0xfeed_beef_dead_c0de,
            segments: vec!["seg/a.zuseg".to_string(), "seg/b.zuseg".to_string()],
        }
    }

    /// Truncates the crc and appends a freshly computed one, so a test can
    /// tamper with the body while keeping the checksum valid.
    fn reseal(bytes: &mut Vec<u8>) {
        bytes.truncate(bytes.len() - 4);
        let crc = crc32c::crc32c(bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn roundtrip() {
        for manifest in [
            Manifest {
                epoch: 0,
                writer_id: 1,
                segments: vec![],
            },
            sample(),
        ] {
            let bytes = manifest.encode().unwrap();
            assert_eq!(Manifest::decode(&bytes).unwrap(), manifest);
        }
    }

    #[test]
    fn every_truncation_is_corrupt() {
        let bytes = sample().encode().unwrap();
        for cut in 0..bytes.len() {
            let err = Manifest::decode(&bytes[..cut]).unwrap_err();
            assert!(
                matches!(err, ZuError::Corrupt { .. }),
                "cut at {cut}: {err}"
            );
        }
    }

    #[test]
    fn bad_magic_is_corrupt() {
        let mut bytes = sample().encode().unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ZuError::Corrupt { .. })
        ));
    }

    #[test]
    fn bad_crc_is_corrupt() {
        let good = sample().encode().unwrap();
        // Flip one payload bit at a time; the checksum must catch every one.
        for i in 6..good.len() - 4 {
            let mut bytes = good.clone();
            bytes[i] ^= 0x01;
            let err = Manifest::decode(&bytes).unwrap_err();
            assert!(matches!(err, ZuError::Corrupt { .. }), "flip at {i}: {err}");
        }
        // A flipped checksum byte is caught too.
        let mut bytes = good;
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ZuError::Corrupt { .. })
        ));
    }

    #[test]
    fn trailing_bytes_are_corrupt() {
        let mut bytes = sample().encode().unwrap();
        bytes.insert(bytes.len() - 4, 0);
        reseal(&mut bytes);
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ZuError::Corrupt { .. })
        ));
    }

    #[test]
    fn non_utf8_key_is_corrupt() {
        let mut bytes = Manifest {
            epoch: 1,
            writer_id: 2,
            segments: vec!["ab".to_string()],
        }
        .encode()
        .unwrap();
        bytes[DIR_START + 2] = 0xff;
        reseal(&mut bytes);
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ZuError::Corrupt { .. })
        ));
    }

    #[test]
    fn future_format_version_is_unsupported() {
        let mut bytes = sample().encode().unwrap();
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        reseal(&mut bytes);
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ZuError::Unsupported {
                what: "manifest format",
                id: 2
            })
        ));
    }

    #[test]
    fn oversized_segment_key_is_rejected_at_encode() {
        let manifest = Manifest {
            epoch: 1,
            writer_id: 2,
            segments: vec!["k".repeat(usize::from(u16::MAX) + 1)],
        };
        assert!(matches!(
            manifest.encode(),
            Err(ZuError::InvalidArgument(_))
        ));
    }
}
