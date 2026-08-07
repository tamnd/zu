//! The decoder must never panic or overrun on arbitrary bytes; errors are fine.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// A generous stand-in for the segment byte length a real caller knows.
const MAX_BYTES: usize = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    let _ = zu_encoding::fsst::decode(data, MAX_BYTES, &mut out);
});
