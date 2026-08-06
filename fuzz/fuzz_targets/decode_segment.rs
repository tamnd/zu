//! The segment dispatcher and every decoder behind it must never panic or
//! overrun on arbitrary bytes; errors are fine.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    let _ = zu_encoding::segment::decode_any(data, &mut out);
});
