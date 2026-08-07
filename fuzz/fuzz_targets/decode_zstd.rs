//! Neither read path may panic or overrun on arbitrary bytes; errors
//! are fine. The two engines are not compared here: a mutated frame
//! carries no content checksum, so libzstd and ruzstd can both accept
//! it and legitimately disagree on the garbage. zu1 catches that case
//! a layer up, where every segment sits behind a crc32c; agreement on
//! valid frames is covered by the roundtrip tests over encoder output.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// A generous stand-in for the byte length a real caller knows.
const MAX_LEN: usize = 1 << 16;

fuzz_target!(|data: &[u8]| {
    let mut fast = Vec::new();
    let _ = zu_encoding::zstd_leaf::decode(data, MAX_LEN, &mut fast);
    let mut pure = Vec::new();
    let _ = zu_encoding::zstd_leaf::decode_fallback(data, MAX_LEN, &mut pure);
});
