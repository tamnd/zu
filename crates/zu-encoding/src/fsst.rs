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
const TRAIN_ROUNDS: usize = 5;
const SAMPLE_TARGET: usize = 64 << 10;

/// Longest-match lookup at `pos` against the table; returns the code and
/// matched length, or None when the position starts no symbol.
fn best_match(map: &HashMap<&[u8], u8>, bytes: &[u8], pos: usize) -> Option<(u8, usize)> {
    let limit = MAX_SYMBOL_LEN.min(bytes.len() - pos);
    for len in (1..=limit).rev() {
        if let Some(&code) = map.get(&bytes[pos..pos + len]) {
            return Some((code, len));
        }
    }
    None
}

/// A sample of evenly spaced chunks so a table trained on a long buffer
/// still sees its tail, capped near 64 KiB to keep training flat-cost.
fn sample(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() <= SAMPLE_TARGET {
        return bytes.to_vec();
    }
    let chunk = 4 << 10;
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
        let map: HashMap<&[u8], u8> = table
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_slice(), i as u8))
            .collect();
        let mut gain: HashMap<Vec<u8>, u64> = HashMap::new();
        let mut prev: Option<(usize, usize)> = None;
        let mut pos = 0;
        while pos < sample.len() {
            let len = best_match(&map, sample, pos).map_or(1, |(_, len)| len);
            *gain.entry(sample[pos..pos + len].to_vec()).or_insert(0) += len as u64;
            if let Some((p_pos, p_len)) = prev
                && p_len + len <= MAX_SYMBOL_LEN
            {
                *gain.entry(sample[p_pos..pos + len].to_vec()).or_insert(0) += (p_len + len) as u64;
            }
            prev = Some((pos, len));
            pos += len;
        }
        let mut candidates: Vec<(Vec<u8>, u64)> = gain.into_iter().collect();
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        table = candidates
            .into_iter()
            .take(MAX_SYMBOLS)
            .map(|(s, _)| s)
            .collect();
    }
    table
}

/// Encodes `bytes` into `out`, returning the encoded byte length.
pub fn encode(bytes: &[u8], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let table = train(&sample(bytes));
    let map: HashMap<&[u8], u8> = table
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_slice(), i as u8))
        .collect();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.push(table.len() as u8);
    for sym in &table {
        out.push(sym.len() as u8);
    }
    for sym in &table {
        out.extend_from_slice(sym);
    }
    let mut pos = 0;
    while pos < bytes.len() {
        match best_match(&map, bytes, pos) {
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
    while i < payload.len() {
        let code = payload[i] as usize;
        i += 1;
        if code < count {
            dst[n..n + MAX_SYMBOL_LEN].copy_from_slice(&table[code]);
            n += tlen[code] as usize;
        } else if code == ESCAPE as usize {
            let Some(&literal) = payload.get(i) else {
                bad = Some("payload ends inside an escape");
                break;
            };
            i += 1;
            dst[n] = literal;
            n += 1;
        } else {
            bad = Some("code past the symbol count");
            break;
        }
        if n > raw_len {
            bad = Some("decoded past the claimed length");
            break;
        }
    }
    if bad.is_none() && n != raw_len {
        bad = Some("decoded length below the claim");
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
    fn training_is_deterministic() {
        let text = text_corpus();
        let mut a = Vec::new();
        let mut b = Vec::new();
        encode(&text, &mut a);
        encode(&text, &mut b);
        assert_eq!(a, b);
    }
}
