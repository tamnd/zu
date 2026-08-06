//! Lightweight columnar encodings.
//!
//! Every encoding has a format-stable id (`docs/04-storage-zu1-format.md` §3)
//! and must decode at 1 GB/s/core or better with at most 64 KiB of scratch.
//! Encoder and decoder implementations land in M0; this crate starts with the
//! stable id table so downstream crates can reference it.

use zu_common::ZuError;

/// Format-stable encoding identifiers. Never renumber.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum EncodingId {
    Plain = 0,
    Constant = 1,
    Rle = 2,
    Dict = 3,
    ForBitPack = 4,
    DeltaBitPack = 5,
    Alp = 6,
    AlpRd = 7,
    Fsst = 8,
    BoolBitpack = 9,
    Frequency = 10,
    Zstd = 11,
}

impl TryFrom<u8> for EncodingId {
    type Error = ZuError;

    fn try_from(v: u8) -> Result<Self, ZuError> {
        Ok(match v {
            0 => Self::Plain,
            1 => Self::Constant,
            2 => Self::Rle,
            3 => Self::Dict,
            4 => Self::ForBitPack,
            5 => Self::DeltaBitPack,
            6 => Self::Alp,
            7 => Self::AlpRd,
            8 => Self::Fsst,
            9 => Self::BoolBitpack,
            10 => Self::Frequency,
            11 => Self::Zstd,
            other => {
                return Err(ZuError::Unsupported {
                    what: "encoding",
                    id: u32::from(other),
                });
            }
        })
    }
}

/// Bits required to represent `max` with frame-of-reference packing.
#[inline]
pub fn bits_needed(max: u64) -> u32 {
    64 - max.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_id_roundtrip() {
        for raw in 0u8..=11 {
            let id = EncodingId::try_from(raw).unwrap();
            assert_eq!(id as u8, raw);
        }
        assert!(EncodingId::try_from(12).is_err());
    }

    #[test]
    fn bits() {
        assert_eq!(bits_needed(0), 0);
        assert_eq!(bits_needed(1), 1);
        assert_eq!(bits_needed(255), 8);
        assert_eq!(bits_needed(256), 9);
        assert_eq!(bits_needed(u64::MAX), 64);
    }
}
