//! FSST string compression (encoding id 8), the Boncz, Neumann and Leis
//! static symbol table: up to 255 symbols of 1 to 8 bytes replace common
//! substrings with one-byte codes, and code 255 escapes a literal byte.
//! Decompression is a table lookup per code with no entropy coding, which
//! is what lets it hit the 1 GB/s decode floor while still halving
//! typical text; random access into a compressed segment only needs the
//! 2 KiB table, not the whole block, which is why FullZip string
//! segments will ride it.
//!
//! The encoder trains on a sample with the iterative construction from
//! the paper: parse the sample with the current table, credit each
//! matched symbol and each concatenation of adjacent matches with the
//! bytes it would save, and keep the highest earners for the next round.
//! Operates on a flat byte buffer; string boundaries are structural
//! (offsets live with the FullZip layout) and no symbol ever crosses a
//! value boundary in practice because the training sample keeps them
//! rare and correctness never depends on it.
//!
//! Layout: `raw_len: u32 LE`, `symbol_count: u8`, one length byte per
//! symbol (1 to 8), the concatenated symbol bytes, then the code stream.

use std::collections::HashMap;

use zu_common::{Result, ZuError};

const MAX_SYMBOLS: usize = 255;
const MAX_SYMBOL_LEN: usize = 8;
const ESCAPE: u8 = 255;
// Symbols double in length once per round, so eight-byte symbols need
// rounds to grow single bytes into pairs, fours, then eights, plus a
// couple of rounds for the survivors to settle against each other.
const TRAIN_ROUNDS: usize = 7;
const SAMPLE_TARGET: usize = 64 << 10;
/// No symbol, in a chain link or a one-byte slot.
const NONE: u16 = u16::MAX;

/// A symbol table arranged for longest-match lookup: a slot per byte for
/// the one-byte symbols, and a chain per two-byte prefix for the rest,
/// each chain ordered longest symbol first so the first match found is
/// the match.
///
/// The obvious version of this is a `HashMap<Vec<u8>, u8>` probed once
/// per length from eight down to one, and that is what this was. It
/// costs up to eight sip hashes of the bytes at every position of every
/// buffer trained on or encoded, and it dominated writing a string
/// column: a million short strings took 4.19s to encode, nearly all of
/// it hashing. Two bytes of the input pick a chain that almost always
/// holds one symbol, so a lookup is an index and a comparison of at
/// most eight bytes, the same million strings take 1.14s, and the
/// bytes written come out the same to the byte.
struct Index {
    syms: Vec<Vec<u8>>,
    /// Code of the one-byte symbol for each byte value, or [`NONE`].
    single: [u16; 256],
    /// First symbol of the chain for each two-byte prefix, or [`NONE`].
    heads: Box<[u16]>,
    /// Next symbol in the chain, parallel to `syms`.
    next: Vec<u16>,
}

impl Index {
    fn new(syms: Vec<Vec<u8>>) -> Index {
        let mut single = [NONE; 256];
        let mut heads = vec![NONE; 1 << 16].into_boxed_slice();
        let mut next = vec![NONE; syms.len()];
        // Shortest first, pushing each onto the front of its chain, so
        // every chain comes out longest first.
        let mut order: Vec<u16> = (0..syms.len() as u16).collect();
        order.sort_by_key(|&i| syms[i as usize].len());
        for i in order {
            let sym = &syms[i as usize];
            if sym.len() == 1 {
                single[sym[0] as usize] = i;
            } else {
                let bucket = ((sym[0] as usize) << 8) | sym[1] as usize;
                next[i as usize] = heads[bucket];
                heads[bucket] = i;
            }
        }
        Index {
            syms,
            single,
            heads,
            next,
        }
    }

    /// Longest-match lookup at `pos`; returns the code and matched
    /// length, or None when the position starts no symbol.
    fn best_match(&self, bytes: &[u8], pos: usize) -> Option<(u8, usize)> {
        let rest = &bytes[pos..];
        if rest.len() >= 2 {
            let bucket = ((rest[0] as usize) << 8) | rest[1] as usize;
            let mut i = self.heads[bucket];
            while i != NONE {
                let sym = &self.syms[i as usize];
                if rest.starts_with(sym) {
                    return Some((i as u8, sym.len()));
                }
                i = self.next[i as usize];
            }
        }
        match self.single[rest[0] as usize] {
            NONE => None,
            code => Some((code as u8, 1)),
        }
    }
}

/// A candidate symbol during training: up to eight bytes and a length,
/// which is a fixed size key rather than the `Vec` it stands for. The
/// training loop proposes one candidate per position and one per pair of
/// adjacent positions, so the `Vec` was two allocations and two hashes
/// of a heap pointer's worth of bytes per position.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Candidate {
    bytes: [u8; MAX_SYMBOL_LEN],
    len: u8,
}

impl Candidate {
    fn new(bytes: &[u8]) -> Candidate {
        let mut out = [0u8; MAX_SYMBOL_LEN];
        out[..bytes.len()].copy_from_slice(bytes);
        Candidate {
            bytes: out,
            len: bytes.len() as u8,
        }
    }

    fn to_vec(self) -> Vec<u8> {
        self.bytes[..self.len as usize].to_vec()
    }
}

/// FNV over the nine bytes of a [`Candidate`], with a final mix so the
/// low bits a hash table indexes with carry the whole key. Sip hashing
/// keys this small is most of what training used to cost, and nothing
/// here is exposed to a chosen key: the input is bytes of a sample the
/// writer drew itself, and a bad round of collisions costs time and not
/// correctness.
#[derive(Default)]
struct SymHasher(u64);

impl std::hash::Hasher for SymHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 29;
        x
    }
}

type SymHash = std::hash::BuildHasherDefault<SymHasher>;

/// A sample of evenly spaced chunks so a table trained on a long buffer
/// still sees its tail, capped near 64 KiB to keep training flat-cost.
fn sample(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() <= SAMPLE_TARGET {
        return bytes.to_vec();
    }
    // 512-byte fragments, following the reference implementation: a
    // symbol is at most 8 bytes, so a fragment carries plenty of
    // adjacency context, and many scattered fragments keep the table
    // from memorizing whatever ids one region of the input holds.
    let chunk = 512;
    let chunks = SAMPLE_TARGET / chunk;
    let stride = bytes.len() / chunks;
    let mut out = Vec::with_capacity(SAMPLE_TARGET);
    for i in 0..chunks {
        let start = i * stride;
        out.extend_from_slice(&bytes[start..bytes.len().min(start + chunk)]);
    }
    out
}

/// Iterative table construction: each round parses the sample with the
/// current table and proposes concatenations of adjacent matches, and
/// the candidates that would save the most bytes survive. Ties break on
/// the symbol bytes so training is deterministic.
fn train(sample: &[u8]) -> Vec<Vec<u8>> {
    let mut table: Vec<Vec<u8>> = Vec::new();
    for _ in 0..TRAIN_ROUNDS {
        let index = Index::new(table);
        let mut gain: HashMap<Candidate, u64, SymHash> = HashMap::default();
        let mut prev: Option<(usize, usize)> = None;
        let mut pos = 0;
        while pos < sample.len() {
            let len = index.best_match(sample, pos).map_or(1, |(_, len)| len);
            *gain
                .entry(Candidate::new(&sample[pos..pos + len]))
                .or_insert(0) += len as u64;
            if let Some((p_pos, p_len)) = prev
                && p_len + len <= MAX_SYMBOL_LEN
            {
                *gain
                    .entry(Candidate::new(&sample[p_pos..pos + len]))
                    .or_insert(0) += (p_len + len) as u64;
            }
            prev = Some((pos, len));
            pos += len;
        }
        let mut candidates: Vec<(Vec<u8>, u64)> =
            gain.into_iter().map(|(c, g)| (c.to_vec(), g)).collect();
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        table = candidates
            .into_iter()
            .take(MAX_SYMBOLS)
            .map(|(s, _)| s)
            .collect();
    }
    table
}

/// A trained symbol table, separated from the encode pass because the two
/// cost wildly different amounts: training a table for one 1024-row chunk
/// of short strings runs about 3.4ms and encoding that chunk against it
/// runs about 0.13ms, so a writer that trains per chunk spends 96% of its
/// time in training. A writer holding a column of chunks that are alike
/// trains once, on a sample drawn across the whole column, and encodes
/// every chunk with the result. Nothing about the encoded form changes:
/// each buffer still carries the table it was encoded with, so a decoder
/// cannot tell which of the two a writer did.
pub struct Table {
    index: Index,
}

impl Table {
    /// Trains on `bytes`, sampling it first when it is long.
    pub fn train(bytes: &[u8]) -> Table {
        Table {
            index: Index::new(train(&sample(bytes))),
        }
    }

    /// The symbols, in code order.
    fn symbols(&self) -> &[Vec<u8>] {
        &self.index.syms
    }

    /// What every encoded buffer pays to carry this table: the fixed
    /// header, one length byte per symbol and the symbol bytes. A writer
    /// comparing two encodings of different sizes has to take this out
    /// first, because a table costing 2 KiB is nothing against a whole
    /// column and a seventh of one 1024-row chunk of short strings.
    pub fn header_len(&self) -> usize {
        5 + self.symbols().len() + self.symbols().iter().map(Vec::len).sum::<usize>()
    }

    /// Encodes `bytes` into `out`, returning the encoded byte length.
    pub fn encode(&self, bytes: &[u8], out: &mut Vec<u8>) -> usize {
        let start = out.len();
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.push(self.symbols().len() as u8);
        for sym in self.symbols() {
            out.push(sym.len() as u8);
        }
        for sym in self.symbols() {
            out.extend_from_slice(sym);
        }
        let mut pos = 0;
        while pos < bytes.len() {
            match self.index.best_match(bytes, pos) {
                Some((code, len)) => {
                    out.push(code);
                    pos += len;
                }
                None => {
                    out.push(ESCAPE);
                    out.push(bytes[pos]);
                    pos += 1;
                }
            }
        }
        out.len() - start
    }
}

/// Trains a table on `bytes` and encodes `bytes` with it, returning the
/// encoded byte length. [`Table`] is the form to reach for when more than
/// one buffer is going to be encoded the same way.
pub fn encode(bytes: &[u8], out: &mut Vec<u8>) -> usize {
    Table::train(bytes).encode(bytes, out)
}

/// Decodes an encoded buffer, appending at most `max_bytes` bytes to
/// `out`. The claimed length, every symbol length, and every code are
/// checked, and a rejected payload leaves `out` untouched. Scratch is
/// the 255-entry padded table, well under the 64 KiB decoder ceiling.
pub fn decode(bytes: &[u8], max_bytes: usize, out: &mut Vec<u8>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "fsst",
        detail: detail.to_string(),
    };
    let header = bytes.get(..5).ok_or_else(|| corrupt("truncated header"))?;
    let raw_len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let count = header[4] as usize;
    if raw_len > max_bytes {
        return Err(corrupt("length above the caller ceiling"));
    }
    let lens = bytes
        .get(5..5 + count)
        .ok_or_else(|| corrupt("truncated symbol lengths"))?;
    if lens.iter().any(|&l| l == 0 || l as usize > MAX_SYMBOL_LEN) {
        return Err(corrupt("symbol length outside 1..=8"));
    }
    let total: usize = lens.iter().map(|&l| l as usize).sum();
    let syms = bytes
        .get(5 + count..5 + count + total)
        .ok_or_else(|| corrupt("truncated symbol bytes"))?;
    let payload = &bytes[5 + count + total..];
    // Every entry is padded to 8 bytes so the hot loop is always one
    // fixed-size copy, and the lengths live in a fixed array so a code
    // that passed the count check indexes without another bounds test.
    let mut table = [[0u8; MAX_SYMBOL_LEN]; MAX_SYMBOLS];
    let mut tlen = [0u8; MAX_SYMBOLS];
    let mut sym_pos = 0;
    for (i, &l) in lens.iter().enumerate() {
        table[i][..l as usize].copy_from_slice(&syms[sym_pos..sym_pos + l as usize]);
        tlen[i] = l;
        sym_pos += l as usize;
    }
    // The output is sized up front from the validated claim, with 8
    // bytes of slack so every symbol write stays in bounds while n is
    // at most raw_len, which the overshoot check below maintains. That
    // turns the hot loop into indexed stores with no capacity checks or
    // length updates; hostile payloads cost at most one iteration past
    // the claim before the error path restores `out` exactly.
    let base = out.len();
    out.resize(base + raw_len + MAX_SYMBOL_LEN, 0);
    let dst = &mut out[base..];
    let mut n = 0;
    let mut i = 0;
    let mut bad: Option<&str> = None;
    // The write window doubles as the overshoot check: the slack sizing
    // makes get_mut fail exactly when n has passed the claimed length,
    // so the hot path is one compare, one 8-byte copy, one add. n can
    // finish up to 7 bytes past the claim through the slack; the final
    // equality check catches that with the rest.
    while i < payload.len() {
        let code = payload[i] as usize;
        i += 1;
        if code < count {
            let Some(w) = dst.get_mut(n..n + MAX_SYMBOL_LEN) else {
                bad = Some("decoded past the claimed length");
                break;
            };
            w.copy_from_slice(&table[code]);
            n += tlen[code] as usize;
        } else if code == ESCAPE as usize {
            let Some(&literal) = payload.get(i) else {
                bad = Some("payload ends inside an escape");
                break;
            };
            i += 1;
            let Some(slot) = dst.get_mut(n) else {
                bad = Some("decoded past the claimed length");
                break;
            };
            *slot = literal;
            n += 1;
        } else {
            bad = Some("code past the symbol count");
            break;
        }
    }
    if bad.is_none() && n != raw_len {
        bad = Some("decoded length disagrees with the claim");
    }
    if let Some(detail) = bad {
        out.truncate(base);
        return Err(corrupt(detail));
    }
    out.truncate(base + raw_len);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_corpus() -> Vec<u8> {
        let mut text = Vec::new();
        for i in 0..4000u64 {
            text.extend_from_slice(
                format!(
                    "https://example.com/user/{}/posts?page={}\n",
                    i * 37,
                    i % 12
                )
                .as_bytes(),
            );
        }
        text
    }

    #[test]
    fn roundtrip_compresses_text() {
        let text = text_corpus();
        let mut buf = Vec::new();
        let len = encode(&text, &mut buf);
        assert!(
            len * 2 < text.len(),
            "repetitive urls should halve, got {len} from {}",
            text.len()
        );
        let mut out = Vec::new();
        decode(&buf, text.len(), &mut out).unwrap();
        assert_eq!(text, out);
    }

    #[test]
    fn roundtrip_edges() {
        let noise: Vec<u8> = (0..2048u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        for bytes in [vec![], vec![7u8], vec![0xFF; 300], noise] {
            let mut buf = Vec::new();
            encode(&bytes, &mut buf);
            let mut out = Vec::new();
            decode(&buf, bytes.len(), &mut out).unwrap();
            assert_eq!(bytes, out);
        }
    }

    #[test]
    fn corrupt_and_hostile() {
        let mut out = vec![1u8, 2, 3];
        assert!(decode(&[1, 2], 16, &mut out).is_err());
        // A flood claim dies on the ceiling before anything materializes.
        let mut buf = u32::MAX.to_le_bytes().to_vec();
        buf.push(0);
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
        // Zero and oversized symbol lengths are rejected.
        for bad_len in [0u8, 9] {
            let mut buf = 1u32.to_le_bytes().to_vec();
            buf.extend_from_slice(&[1, bad_len, b'x', 0]);
            assert!(decode(&buf, 16, &mut out).is_err());
        }
        // A code past the symbol count, a payload ending inside an
        // escape, and a stream that misses its claimed length are all
        // Corrupt errors that leave `out` exactly as it was.
        for payload in [&[5u8][..], &[ESCAPE], &[]] {
            let mut buf = 1u32.to_le_bytes().to_vec();
            buf.extend_from_slice(&[1, 1, b'x']);
            buf.extend_from_slice(payload);
            assert!(decode(&buf, 16, &mut out).is_err());
        }
        // Overshoot: two 8-byte symbols against a claim of 9.
        let mut buf = 9u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[1, 8]);
        buf.extend_from_slice(b"abcdefgh");
        buf.extend_from_slice(&[0, 0]);
        assert!(decode(&buf, 64, &mut out).is_err());
        assert_eq!(out, [1, 2, 3], "a rejected payload must not touch out");
    }

    #[test]
    fn a_table_encodes_buffers_it_did_not_train_on() {
        // What a column writer does: train once, encode every chunk with
        // it. A buffer the table never saw still has to round-trip, both
        // when it is more of the same and when it shares nothing with the
        // sample and every byte escapes.
        let table = Table::train(&text_corpus());
        for text in [
            b"https://example.com/user/999999/posts?page=3".to_vec(),
            (0..=255u8).collect(),
            Vec::new(),
        ] {
            let mut buf = Vec::new();
            let len = table.encode(&text, &mut buf);
            assert_eq!(len, buf.len());
            let mut out = Vec::new();
            decode(&buf, text.len().max(1), &mut out).unwrap();
            assert_eq!(text, out);
        }
    }

    #[test]
    fn training_is_deterministic() {
        let text = text_corpus();
        let mut a = Vec::new();
        let mut b = Vec::new();
        encode(&text, &mut a);
        encode(&text, &mut b);
        assert_eq!(a, b);
    }
}
