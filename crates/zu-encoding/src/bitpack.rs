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
    let mask = (1u64 << width) - 1;
    let mut words = packed[..packed_bytes(width)]
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()));
    let mut cur = words.next().unwrap();
    let mut avail = 64u32;
    for slot in out.iter_mut() {
        *slot = if avail >= width {
            let v = cur & mask;
            cur >>= width;
            avail -= width;
            v
        } else {
            let next = words.next().unwrap();
            let consumed = width - avail;
            let v = (cur | (next << avail)) & mask;
            cur = next >> consumed;
            avail = 64 - consumed;
            v
        };
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
