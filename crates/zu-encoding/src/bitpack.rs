//! Fixed-width bit packing over 1024-value chunks.
//!
//! A full chunk at width 2..=63 uses the FastLanes-style interleaved lane
//! order: value i lives in lane i % 16, and the 16 lanes advance together,
//! so both pack and unpack run 16 identical shift-mask operations on
//! consecutive words per step and auto-vectorize. A full chunk at width w
//! occupies exactly 128 * w bytes, so chunk starts are always word aligned
//! and a point read touches one chunk only. A short tail packs LSB-first
//! sequentially into just the words it needs, which keeps tiny streams such
//! as RLE run values from ballooning to full-chunk size. A tail long enough
//! to fill the full-chunk byte count is zero-padded and transposed like a
//! full chunk, so the packed size alone tells unpack which layout it holds.
//! Width 1 stays sequential in every case: those streams are presence
//! bitmaps whose consumers walk set bits by position, so bit i must mean
//! value i. Width 64 stays a plain little-endian value array.

/// Values per chunk. Format-stable.
pub const CHUNK: usize = 1024;

/// Interleave factor of the transposed layout. Format-stable.
const LANES: usize = 16;

/// Packed size in bytes of `count` values at `width` bits, rounded up to
/// whole words. A full chunk is `128 * width`.
#[inline]
pub const fn packed_bytes(width: u32, count: usize) -> usize {
    (count * width as usize).div_ceil(64) * 8
}

/// Packs up to 1024 values at `width` bits into whole little-endian words.
/// Values must already fit in `width` bits.
pub fn pack(values: &[u64], width: u32, out: &mut Vec<u8>) {
    assert!(values.len() <= CHUNK);
    assert!(width <= 64);
    if width == 0 {
        return;
    }
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    if width > 1 && width < 64 && packed_bytes(width, values.len()) == 128 * width as usize {
        pack_transposed(values, width, mask, out);
        return;
    }
    let mut acc = 0u64;
    let mut bits = 0u32;
    for &raw in values {
        let v = raw & mask;
        acc |= v << bits;
        bits += width;
        if bits >= 64 {
            out.extend_from_slice(&acc.to_le_bytes());
            bits -= 64;
            acc = if bits == 0 { 0 } else { v >> (width - bits) };
        }
    }
    if bits > 0 {
        out.extend_from_slice(&acc.to_le_bytes());
    }
}

/// Full-chunk transposed pack. Word `16 * r + lane` holds bit row r of
/// lane `lane`; value i sits at bit `(i / 16) * width` of lane `i % 16`.
fn pack_transposed(values: &[u64], width: u32, mask: u64, out: &mut Vec<u8>) {
    let w = width as usize;
    let mut words = [0u64; LANES * 63];
    let words = &mut words[..LANES * w];
    for (i, &raw) in values.iter().enumerate() {
        let bit = i / LANES * w;
        let (r, s) = (bit >> 6, (bit & 63) as u32);
        let v = raw & mask;
        words[LANES * r + i % LANES] |= v << s;
        if s + width > 64 {
            words[LANES * (r + 1) + i % LANES] |= v >> (64 - s);
        }
    }
    for &word in words.iter() {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Unpacks one chunk. `out` must be exactly `CHUNK` long; `packed` holds
/// whole words and may cover fewer than 1024 values, in which case the
/// remainder of `out` is zeroed.
///
/// Dispatches to a const-width loop so every shift is a compile-time
/// constant; the generic rolling-window fallback is an order of magnitude
/// slower on older cores and exists only in tests as the oracle.
pub fn unpack(packed: &[u8], width: u32, out: &mut [u64]) {
    assert_eq!(out.len(), CHUNK);
    assert!(width <= 64);
    assert_eq!(packed.len() % 8, 0);
    if width == 0 {
        out.fill(0);
        return;
    }
    if width == 64 {
        let n = (packed.len() / 8).min(CHUNK);
        for (chunk, slot) in packed.chunks_exact(8).zip(out.iter_mut()) {
            *slot = u64::from_le_bytes(chunk.try_into().unwrap());
        }
        out[n..].fill(0);
        return;
    }
    let full = width > 1 && packed.len() == 128 * width as usize;
    macro_rules! dispatch {
        ($($w:literal)*) => {
            match width {
                $($w if full => unpack_transposed::<$w>(packed, out),)*
                $($w => unpack_const::<$w>(packed, out),)*
                _ => unreachable!(),
            }
        };
    }
    dispatch!(
        1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22
        23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44
        45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63
    );
}

/// Full-chunk transposed unpack. Each step reads the same bit position in
/// all 16 lanes, so the lane loop is 16 identical shift-mask operations on
/// consecutive words and vectorizes; the word-crossing branch depends only
/// on the step, never the lane.
fn unpack_transposed<const W: u32>(packed: &[u8], out: &mut [u64]) {
    let mask: u64 = (1u64 << W) - 1;
    let mut words = [0u64; LANES * 63];
    for (slot, chunk) in words[..LANES * W as usize]
        .iter_mut()
        .zip(packed.chunks_exact(8))
    {
        *slot = u64::from_le_bytes(chunk.try_into().unwrap());
    }
    for (t, out16) in out.chunks_exact_mut(LANES).enumerate() {
        let bit = t as u32 * W;
        let r = (bit >> 6) as usize;
        let s = bit & 63;
        let lo = &words[LANES * r..LANES * (r + 1)];
        if s + W > 64 {
            let hi = &words[LANES * (r + 1)..LANES * (r + 2)];
            for ((o, &l), &h) in out16.iter_mut().zip(lo).zip(hi) {
                *o = ((l >> s) | (h << (64 - s))) & mask;
            }
        } else {
            for (o, &l) in out16.iter_mut().zip(lo) {
                *o = (l >> s) & mask;
            }
        }
    }
}

fn unpack_const<const W: u32>(packed: &[u8], out: &mut [u64]) {
    let mask: u64 = (1u64 << W) - 1;
    // 16 words per width bit, 8 KiB at the widest, within the 64 KiB
    // scratch contract.
    let mut words = [0u64; 16 * 63];
    for (slot, chunk) in words[..16 * W as usize]
        .iter_mut()
        .zip(packed.chunks_exact(8))
    {
        *slot = u64::from_le_bytes(chunk.try_into().unwrap());
    }
    // 64 values consume exactly W words, so unroll in blocks of 64 with a
    // repeating in-block shift pattern the compiler can flatten.
    for (block, out_block) in out.chunks_exact_mut(64).enumerate() {
        let base = block * W as usize;
        for (i, slot) in out_block.iter_mut().enumerate() {
            let bit = i as u32 * W;
            let w = base + (bit >> 6) as usize;
            let s = bit & 63;
            let lo = words[w] >> s;
            *slot = if s + W > 64 {
                (lo | (words[w + 1] << (64 - s))) & mask
            } else {
                lo & mask
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn roundtrip_every_width() {
        let mut rng = 0x2545F4914F6CDD1Du64;
        for width in 0..=64u32 {
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            let values: Vec<u64> = (0..CHUNK).map(|_| xorshift(&mut rng) & mask).collect();
            let mut packed = Vec::new();
            pack(&values, width, &mut packed);
            assert_eq!(packed.len(), packed_bytes(width, CHUNK));
            let mut out = vec![0u64; CHUNK];
            unpack(&packed, width, &mut out);
            assert_eq!(values, out, "width {width}");
        }
    }

    #[test]
    fn short_tail_packs_tight() {
        let values = [7u64, 7, 7];
        let mut packed = Vec::new();
        pack(&values, 3, &mut packed);
        assert_eq!(packed.len(), packed_bytes(3, 3));
        assert_eq!(packed.len(), 8);
        let mut out = vec![1u64; CHUNK];
        unpack(&packed, 3, &mut out);
        assert_eq!(&out[..3], &values);
        assert!(out[3..].iter().all(|&v| v == 0));
    }

    #[test]
    fn tail_boundaries_every_width() {
        let mut rng = 0xDEADBEEFu64;
        for width in 1..=64u32 {
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            // 1000 and 1023 cover tails whose packed size collides with
            // the full-chunk size at small widths, which must take the
            // padded transposed layout.
            for len in [1usize, 63, 64, 65, 1000, 1023, CHUNK] {
                let values: Vec<u64> = (0..len).map(|_| xorshift(&mut rng) & mask).collect();
                let mut packed = Vec::new();
                pack(&values, width, &mut packed);
                assert_eq!(packed.len(), packed_bytes(width, len), "w{width} n{len}");
                let mut out = vec![1u64; CHUNK];
                unpack(&packed, width, &mut out);
                assert_eq!(&out[..len], values.as_slice(), "w{width} n{len}");
                assert!(out[len..].iter().all(|&v| v == 0), "w{width} n{len} pad");
            }
        }
    }
}
