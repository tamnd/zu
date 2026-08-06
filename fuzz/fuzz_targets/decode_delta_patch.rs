//! The decoder must never panic or overrun on arbitrary bytes; errors are fine.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// A generous stand-in for the row count a real caller knows.
const MAX_VALUES: usize = 1 << 16;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    let _ = zu_encoding::delta_patch::decode(data, MAX_VALUES, &mut out);
    out.clear();
    let _ = zu_encoding::patch::decode(data, MAX_VALUES, &mut out);
});
