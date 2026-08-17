//! What checksumming a release costs.
//!
//! The digest list is written over every other file of the release, so
//! this runs once per release on a few hundred megabytes of archives,
//! and it runs again on every machine that installs from it. Neither is
//! a hot path, and that is exactly why it is measured: a scalar SHA-256
//! that quietly ran at 20 MiB/s would add a minute to a release nobody
//! would think to look for, and would make `install.sh` feel like it
//! hung on the step that is meant to reassure.
//!
//! The block loop is the whole cost, so the interesting column is the
//! throughput at a size that is not padding: a hasher that is fast at
//! 1 MiB and slow at 16 is one that allocates per block.
//!
//! With ZU_GATE=1 the process exits nonzero when throughput misses the
//! floor in bench/budgets.toml.
//!
//! Run: ZU_GATE=1 cargo bench -p xtask --bench sha256

use std::hint::black_box;
use std::time::Instant;

use xtask::sha256;

/// What a release weighs, near enough: seven platform archives of about
/// twenty megabytes each, plus the model, the header and the corpus.
const RELEASE_MIB: f64 = 150.0;

fn main() {
    println!("{:>10}  {:>9}  {:>10}", "size", "ms", "MiB/s");
    let mut small = None;
    let mut large = 0.0f64;
    for kib in [1usize, 64, 1024, 16 * 1024] {
        let bytes = data(kib * 1024);
        let ms = best(|| {
            black_box(sha256::digest(black_box(&bytes)));
        });
        let mibs = bytes.len() as f64 / 1024.0 / 1024.0 / (ms / 1e3);
        println!("{:>8} KiB  {ms:9.3}  {mibs:10.1}", kib);

        // One KiB is padding and function call, not throughput, so the
        // comparison starts at 64.
        if kib == 64 {
            small = Some(mibs);
        }
        if kib == 16 * 1024 {
            large = mibs;
            let small = small.expect("64 KiB ran first");
            // The same loop over more blocks. Half the throughput at 256
            // times the size is a per-block allocation or a copy of the
            // input, which is the failure a single-size bench misses.
            assert!(
                large > small * 0.5,
                "{large:.1} MiB/s at 16 MiB against {small:.1} MiB/s at 64 KiB, so the cost per \
                 block grows with the input"
            );
        }
    }

    println!(
        "\na release of about {RELEASE_MIB:.0} MiB hashes in {:.2} s",
        RELEASE_MIB / large
    );

    // The hex is what goes in the file, and it is a per-byte loop over
    // thirty-two bytes. It is here because it is the one part of this
    // that could be written to allocate per nibble.
    let ms = best(|| {
        black_box(sha256::hex(black_box(b"the digest of a name in a release")));
    });
    println!("hex of one digest {:.2} us", ms * 1e3);

    if large < floor() {
        eprintln!(
            "GATE sha256_mibs: {large:.1} MiB/s under the {:.1} MiB/s floor",
            floor()
        );
        if std::env::var("ZU_GATE").is_ok_and(|v| v == "1") {
            std::process::exit(1);
        }
    }
}

/// Bytes that are not all the same, since a compressed archive is not
/// and a hasher that was only ever given zeros is one measured on a
/// pattern the branch predictor likes.
fn data(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = 0x2545_f491_4f6c_dd1du64;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// The best of seven, in milliseconds. The best rather than the mean
/// because the thing being measured is the work, and every sample above
/// the floor is the machine doing something else.
fn best(mut body: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        body();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    best
}

/// The floor out of bench/budgets.toml, or nothing to beat when the key
/// is gone.
fn floor() -> f64 {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .find(|(k, _)| k.trim() == "sha256_mibs")
                .and_then(|(_, v)| v.split('#').next()?.trim().parse().ok())
        })
        .unwrap_or(0.0)
}
