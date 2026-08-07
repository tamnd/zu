//! Zstd leaf (encoding id 11), the optional general-purpose stage for
//! cold string segments where ratio matters more than decode speed.
//! Everything else in the cascade is a lightweight encoding; this is
//! the one deliberate exception, and it is a leaf: nothing cascades
//! below it.
//!
//! The `zstd` feature links libzstd for compression and fast decode.
//! Builds without it stay pure Rust and still read every file through
//! ruzstd, so the default build can open a database written by a
//! feature-full one; only `encode` needs the feature. `decode_fallback`
//! is the ruzstd path under its own name so tests and fuzzing can pin
//! it even when libzstd is linked.
//!
//! Layout: `raw_len: u32 LE`, then one zstd frame.

use zu_common::{Result, ZuError};

fn corrupt(detail: &str) -> ZuError {
    ZuError::Corrupt {
        what: "zstd",
        detail: detail.to_string(),
    }
}

/// Compresses `bytes` into `out`, returning the encoded byte length.
#[cfg(feature = "zstd")]
pub fn encode(bytes: &[u8], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    let frame = zstd::bulk::compress(bytes, 3).expect("in-memory compression cannot fail");
    out.extend_from_slice(&frame);
    out.len() - start
}

/// Splits the container into the validated length claim and the frame.
fn claim(bytes: &[u8], max_len: usize) -> Result<(usize, &[u8])> {
    let head = bytes.get(..4).ok_or_else(|| corrupt("truncated header"))?;
    let raw_len = u32::from_le_bytes(head.try_into().unwrap()) as usize;
    if raw_len > max_len {
        return Err(corrupt("length above the caller ceiling"));
    }
    Ok((raw_len, &bytes[4..]))
}

/// Decodes an encoded buffer, appending at most `max_len` bytes to
/// `out`. With the `zstd` feature this is libzstd; without it, ruzstd.
/// A rejected payload leaves `out` untouched.
pub fn decode(bytes: &[u8], max_len: usize, out: &mut Vec<u8>) -> Result<()> {
    #[cfg(feature = "zstd")]
    {
        let (raw_len, frame) = claim(bytes, max_len)?;
        let base = out.len();
        // An empty destination decompresses straight into spare
        // capacity, the WriteBuf path, which skips zeroing the range
        // first; libzstd never reads it. Appending after existing
        // content takes the slice path, since WriteBuf on a Vec always
        // writes from the front.
        let result = if base == 0 {
            out.reserve(raw_len);
            zstd::bulk::Decompressor::new().and_then(|mut d| d.decompress_to_buffer(frame, out))
        } else {
            out.resize(base + raw_len, 0);
            zstd::bulk::decompress_to_buffer(frame, &mut out[base..])
        };
        match result {
            Ok(n) if n == raw_len => {
                out.truncate(base + raw_len);
                Ok(())
            }
            Ok(_) => {
                out.truncate(base);
                Err(corrupt("frame length disagrees with the claim"))
            }
            Err(_) => {
                out.truncate(base);
                Err(corrupt("malformed frame"))
            }
        }
    }
    #[cfg(not(feature = "zstd"))]
    {
        decode_fallback(bytes, max_len, out)
    }
}

/// The pure Rust read path, always available regardless of features.
pub fn decode_fallback(bytes: &[u8], max_len: usize, out: &mut Vec<u8>) -> Result<()> {
    use std::io::Read;
    let (raw_len, frame) = claim(bytes, max_len)?;
    let mut reader = frame;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut reader)
        .map_err(|_| corrupt("malformed frame header"))?;
    let base = out.len();
    out.resize(base + raw_len, 0);
    if decoder.read_exact(&mut out[base..]).is_err() {
        out.truncate(base);
        return Err(corrupt("frame ends before the claimed length"));
    }
    // One byte past the claim distinguishes an exact frame from one
    // that kept going; anything but a clean EOF is a lying claim.
    if !matches!(decoder.read(&mut [0u8]), Ok(0)) {
        out.truncate(base);
        return Err(corrupt("frame length disagrees with the claim"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `printf 'the quick brown fox jumps over the lazy dog; ' x40`
    /// through `zstd -3`, so the pure Rust read path has a real frame
    /// to chew on even when the encoder is compiled out.
    const FIXTURE_RAW_LEN: usize = 1800;
    const FIXTURE_FRAME: &[u8] = &[
        0x28, 0xB5, 0x2F, 0xFD, 0x04, 0x58, 0xBD, 0x01, 0x00, 0xC4, 0x02, 0x74, 0x68, 0x65, 0x20,
        0x71, 0x75, 0x69, 0x63, 0x6B, 0x20, 0x62, 0x72, 0x6F, 0x77, 0x6E, 0x20, 0x66, 0x6F, 0x78,
        0x20, 0x6A, 0x75, 0x6D, 0x70, 0x73, 0x20, 0x6F, 0x76, 0x65, 0x72, 0x20, 0x74, 0x68, 0x65,
        0x20, 0x6C, 0x61, 0x7A, 0x79, 0x20, 0x64, 0x6F, 0x67, 0x3B, 0x02, 0x00, 0xD4, 0x42, 0xF5,
        0x01, 0x43, 0x98, 0x65, 0x0D, 0x2D, 0x33, 0x91,
    ];

    fn fixture() -> Vec<u8> {
        let mut buf = (FIXTURE_RAW_LEN as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(FIXTURE_FRAME);
        buf
    }

    #[test]
    fn fallback_reads_a_real_frame() {
        let mut out = Vec::new();
        decode_fallback(&fixture(), FIXTURE_RAW_LEN, &mut out).unwrap();
        let expected: Vec<u8> = "the quick brown fox jumps over the lazy dog; "
            .bytes()
            .cycle()
            .take(FIXTURE_RAW_LEN)
            .collect();
        assert_eq!(out, expected);
        let mut via_decode = Vec::new();
        decode(&fixture(), FIXTURE_RAW_LEN, &mut via_decode).unwrap();
        assert_eq!(via_decode, out);
    }

    #[test]
    fn corrupt_and_hostile() {
        for decode in [
            decode as fn(&[u8], usize, &mut Vec<u8>) -> Result<()>,
            decode_fallback,
        ] {
            let mut out = vec![7u8];
            assert!(decode(&[1, 2], 16, &mut out).is_err());
            // A flood claim dies on the ceiling before any allocation.
            let mut flood = u32::MAX.to_le_bytes().to_vec();
            flood.extend_from_slice(FIXTURE_FRAME);
            assert!(decode(&flood, 1 << 20, &mut out).is_err());
            // Claims off by one in both directions are rejected.
            for lie in [FIXTURE_RAW_LEN - 1, FIXTURE_RAW_LEN + 1] {
                let mut buf = (lie as u32).to_le_bytes().to_vec();
                buf.extend_from_slice(FIXTURE_FRAME);
                assert!(decode(&buf, 1 << 20, &mut out).is_err());
            }
            // A truncated frame and plain garbage are rejected.
            let mut cut = fixture();
            cut.truncate(cut.len() - 9);
            assert!(decode(&cut, FIXTURE_RAW_LEN, &mut out).is_err());
            let mut garbage = 100u32.to_le_bytes().to_vec();
            garbage.extend_from_slice(&[0xAB; 40]);
            assert!(decode(&garbage, 100, &mut out).is_err());
            assert_eq!(out, [7], "a rejected payload must not touch out");
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn roundtrip_both_read_paths() {
        let text: Vec<u8> = (0..2000u32)
            .flat_map(|i| format!("{i}\t{}\n", i * 37).into_bytes())
            .collect();
        for values in [Vec::new(), vec![0u8; 10], text] {
            let mut buf = Vec::new();
            let len = encode(&values, &mut buf);
            assert_eq!(len, buf.len());
            let mut fast = Vec::new();
            decode(&buf, values.len(), &mut fast).unwrap();
            assert_eq!(fast, values);
            let mut pure = Vec::new();
            decode_fallback(&buf, values.len(), &mut pure).unwrap();
            assert_eq!(pure, values);
        }
    }
}
