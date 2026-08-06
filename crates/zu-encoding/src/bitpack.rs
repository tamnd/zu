//! Fixed-width bit packing over 1024-value chunks.
//!
//! Values are packed LSB-first into little-endian u64 words. A chunk of 1024
//! values at width w occupies exactly 128 * w bytes, so chunk starts are
//! always word aligned and a point read touches one chunk only.
//! The FastLanes transposed lane order is a planned swap behind this same
//! interface before format freeze; the container layout does not change.

/// Values per chunk. Format-stable.
pub const CHUNK: usize = 1024;

/// Packed size in bytes of one chunk at `width` bits.
#[inline]
pub const fn packed_bytes(width: u32) -> usize {
    CHUNK / 8 * width as usize
}

/// Packs up to 1024 values at `width` bits, zero-padding a short tail.
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
    let mut acc = 0u64;
    let mut bits = 0u32;
    for i in 0..CHUNK {
        let v = values.get(i).copied().unwrap_or(0) & mask;
        acc |= v << bits;
        bits += width;
        if bits >= 64 {
            out.extend_from_slice(&acc.to_le_bytes());
            bits -= 64;
            acc = if bits == 0 { 0 } else { v >> (width - bits) };
        }
    }
    debug_assert_eq!(bits, 0);
}

/// Unpacks one chunk. `out` must be exactly `CHUNK` long and `packed` at
/// least `packed_bytes(width)`.
///
/// Dispatches to a const-width loop so every shift is a compile-time
/// constant; the generic rolling-window fallback is an order of magnitude
/// slower on older cores and exists only in tests as the oracle.
pub fn unpack(packed: &[u8], width: u32, out: &mut [u64]) {
    assert_eq!(out.len(), CHUNK);
    assert!(width <= 64);
    if width == 0 {
        out.fill(0);
        return;
    }
    assert!(packed.len() >= packed_bytes(width));
    if width == 64 {
        for (chunk, slot) in packed.chunks_exact(8).zip(out.iter_mut()) {
            *slot = u64::from_le_bytes(chunk.try_into().unwrap());
        }
        return;
    }
    macro_rules! dispatch {
        ($($w:literal)*) => {
            match width {
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
            assert_eq!(packed.len(), packed_bytes(width));
            let mut out = vec![0u64; CHUNK];
            unpack(&packed, width, &mut out);
            assert_eq!(values, out, "width {width}");
        }
    }

    #[test]
    fn short_tail_pads_with_zeros() {
        let values = [7u64, 7, 7];
        let mut packed = Vec::new();
        pack(&values, 3, &mut packed);
        let mut out = vec![0u64; CHUNK];
        unpack(&packed, 3, &mut out);
        assert_eq!(&out[..3], &values);
        assert!(out[3..].iter().all(|&v| v == 0));
    }
}
