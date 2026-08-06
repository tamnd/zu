//! Feeds arbitrary bytes to `zu verify` as a whole database file: open,
//! header pick, every meta chain, the catalog, the table index, each
//! directory with all four segments per group, and the free list. Any
//! input may fail with an error; none may panic, hang, or allocate
//! without bound. Seed with `cargo run --bin seeds` so mutation starts
//! from a valid file; the crc32c walls mean random bytes only reach the
//! header layer, which is why the raw decoders fuzz separately in
//! `zu1_decode`.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

fn scratch_path() -> &'static PathBuf {
    static DIR: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, path) = DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("fuzz.zu1");
        (dir, path)
    });
    path
}

fuzz_target!(|data: &[u8]| {
    let path = scratch_path();
    std::fs::write(path, data).expect("write scratch file");
    let _ = zu_zu1::verify(path);
});
