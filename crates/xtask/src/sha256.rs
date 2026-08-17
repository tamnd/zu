//! SHA-256, because a release that publishes no checksums is an
//! install one-liner that pipes an unverified download into a shell.
//!
//! Written here rather than taken as a dependency for the same reason
//! the tar writer is: this is two hundred lines of arithmetic that has
//! not changed since 2001 and is pinned by published test vectors, and
//! a build tool that reaches for a crate to hash ten files has added a
//! supply chain to the thing whose whole job is to be the trusted end
//! of one. The vectors below are FIPS 180-4's own, plus the million `a`
//! that is the only case long enough to exercise the length encoding.
//!
//! It is not a general-purpose hasher and does not pretend to be. There
//! is no streaming API, because every caller here has the bytes in
//! memory already, and no constant-time promise, because a checksum in
//! a release is a public number.

/// The digest, as the sixty-four hex characters `sha256sum` writes.
///
/// Lowercase hex with no prefix, which is what every tool that reads a
/// `SHA256SUMS` file expects and what a user comparing two of them by
/// eye can compare.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest(bytes) {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is a hex digit"));
        out.push(char::from_digit(u32::from(byte & 0xf), 16).expect("a nibble is a hex digit"));
    }
    out
}

/// The thirty-two bytes themselves.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut state = INIT;
    let mut block = [0u8; 64];

    let whole = bytes.len() / 64;
    for i in 0..whole {
        block.copy_from_slice(&bytes[i * 64..i * 64 + 64]);
        compress(&mut state, &block);
    }

    // The padding: the remainder, a one bit, zeros, and the length in
    // bits as a big-endian u64 in the last eight bytes. When the
    // remainder leaves no room for that length, it takes a block of its
    // own, which is the case a hasher that was never given 56 to 63
    // trailing bytes gets wrong and nothing notices.
    let rest = &bytes[whole * 64..];
    block = [0u8; 64];
    block[..rest.len()].copy_from_slice(rest);
    block[rest.len()] = 0x80;
    if rest.len() >= 56 {
        compress(&mut state, &block);
        block = [0u8; 64];
    }
    let bits = (bytes.len() as u64) * 8;
    block[56..].copy_from_slice(&bits.to_be_bytes());
    compress(&mut state, &block);

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// One 64-byte block into the state.
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}

/// The fractional parts of the square roots of the first eight primes.
const INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// The fractional parts of the cube roots of the first sixty-four.
#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4's own vectors, which is what makes this an
    /// implementation of SHA-256 rather than of something that looks
    /// like it.
    #[test]
    fn the_published_vectors_hash_to_what_they_publish() {
        for (input, want) in [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno\
                 ijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ] {
            assert_eq!(hex(input.as_bytes()), want, "{input:?}");
        }
    }

    /// The one vector long enough to carry a length that does not fit in
    /// a byte, and long enough to run the block loop rather than the
    /// padding alone.
    #[test]
    fn a_million_letters_hash_to_the_published_digest() {
        let input = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&input),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Every remainder from 0 to 64 inclusive, because the padding
    /// branches at 56 and a hasher that never saw 56 to 63 trailing
    /// bytes is a hasher that agrees with everybody else until the day
    /// a release artifact happens to be that length.
    #[test]
    fn every_length_around_the_padding_boundary_agrees_with_a_second_reading() {
        // The second reading is the same function over the same bytes
        // split differently: hashing n bytes has to be what hashing
        // n bytes concatenated from two halves gives, which is only
        // true if the padding is placed by length and not by luck.
        for n in 0..=128usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 7 + 1) as u8).collect();
            let once = digest(&bytes);
            let mut joined = bytes[..n / 2].to_vec();
            joined.extend_from_slice(&bytes[n / 2..]);
            assert_eq!(once, digest(&joined), "{n}");
        }
    }

    #[test]
    fn the_digest_is_written_the_way_sha256sum_writes_it() {
        let text = hex(b"zu");
        assert_eq!(text.len(), 64);
        assert!(
            text.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{text}"
        );
    }
}
