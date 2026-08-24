//! What compressing a cold tier record would cost and what it would
//! save.
//!
//! #725 proposes compressing [`crate::cold`] and nothing else. The
//! argument is that a record only reaches the tier by surviving a whole
//! lap of the log untouched, and a read of one is a `pread` of a device
//! that answers in tens to hundreds of microseconds, so a decompress
//! measured in single microseconds is invisible there in a way it would
//! never be in the hot log, where a point read is the whole product.
//!
//! That argument has a number in it and the number has not been
//! measured. This measures it. The gate #725 set on itself is that
//! decompress is not a visible fraction of a cold read and that the
//! ratio on real record sizes is better than about 0.85, and it says
//! that if either fails the format bump is not worth it.
//!
//! The ratio half was settled in Python before this existed and is
//! repeated here so the two can be checked against each other. Per
//! record deflate on go-ycsb's own value alphabet was 1.019 at a
//! hundred bytes, 0.748 at a thousand and 0.731 at four thousand, and
//! grouping records into a two megabyte block only reached 0.728, which
//! is why the tier can stay one record to a `pread`. If this file
//! disagrees with those, this file is the one to believe: it uses the
//! coders zu would actually link.
//!
//! The values are drawn the way `go-ycsb` draws them, from the fifty
//! two letters at `pkg/util/util.go:40`. That is not an arbitrary
//! choice of test data, it is the data every number in the benchmark
//! series was measured on, and it carries 5.70 bits a byte, so 0.713 is
//! the floor no coder beats.
//!
//! Run: cargo bench -p zu2 --bench compress

use std::time::Instant;

/// Records a cell, enough that a timing is not one cache warm up.
const RECORDS: usize = 2000;

/// Passes over the set, best of, for the reason the scan bench gives:
/// on a loaded machine one pass moves more than the effect being
/// measured, and the fastest pass is the closest this gets to the
/// coder on its own.
const REPEATS: usize = 5;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// go-ycsb's value generator, `pkg/util/util.go:40`.
fn values(count: usize, bytes: usize, seed: u64) -> Vec<Vec<u8>> {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut rng = Rng(seed);
    (0..count)
        .map(|_| {
            (0..bytes)
                .map(|_| LETTERS[(rng.next() % LETTERS.len() as u64) as usize])
                .collect()
        })
        .collect()
}

struct Row {
    coder: &'static str,
    bytes: usize,
    raw: usize,
    coded: usize,
    encode_ns: f64,
    decode_ns: f64,
}

fn main() {
    let sizes: Vec<usize> = std::env::var("ZU2_SIZES")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![100, 500, 1000, 2000, 4000]);

    println!("# zu2 cold tier compression, per record, see tamnd/zu#725");
    println!(
        "# {RECORDS} records a cell, best of {REPEATS}, go-ycsb's fifty two letter \
         value alphabet, entropy floor 0.713"
    );
    println!("coder\trec_bytes\tratio\tencode_ns\tdecode_ns\tMiB_per_1M_records");

    let mut rows: Vec<Row> = Vec::new();

    for &bytes in &sizes {
        let set = values(RECORDS, bytes, 0x9E3779B97F4A7C15 ^ bytes as u64);
        let raw: usize = set.iter().map(Vec::len).sum();

        // zstd at level 3, which is its default and the level anything
        // on a read path would use. The frame carries a magic number and
        // a header, and at a thousand bytes that overhead is a real
        // fraction, which is the whole reason level 19 barely beats 3
        // here and why deflate is worth comparing against at all.
        for level in [1i32, 3, 9] {
            // `zstd::encode_all` and `decode_all` build a context per
            // call, and the first version of this bench used them and
            // reported twenty five microseconds to decode a thousand
            // byte record, which is forty megabytes a second out of a
            // coder that does gigabytes. Nearly all of it was the
            // context. The bulk API keeps one and reuses it, which is
            // what an engine coding a record at a time would do and the
            // only shape in which this proposal makes any sense.
            let mut encoder = zstd::bulk::Compressor::new(level).expect("zstd compressor");
            let mut decoder = zstd::bulk::Decompressor::new().expect("zstd decompressor");
            let mut coded = 0usize;
            let mut encode = f64::MAX;
            let mut decode = f64::MAX;
            let mut out: Vec<Vec<u8>> = Vec::new();
            // One buffer for the decode, reused, for the same reason:
            // an allocation a record would be measured as coder time
            // and is not.
            let mut back = vec![0u8; bytes + 64];
            for _ in 0..REPEATS {
                let t = Instant::now();
                out = set
                    .iter()
                    .map(|v| encoder.compress(v).expect("zstd encode"))
                    .collect();
                encode = encode.min(t.elapsed().as_secs_f64());
                coded = out.iter().map(Vec::len).sum();

                let t = Instant::now();
                let mut held = 0usize;
                for c in &out {
                    held += decoder
                        .decompress_to_buffer(c, &mut back)
                        .expect("zstd decode");
                }
                std::hint::black_box(held);
                decode = decode.min(t.elapsed().as_secs_f64());
            }
            // The round trip is checked once rather than never. A coder
            // that loses bytes would otherwise look like a very good
            // ratio.
            for (c, v) in out.iter().zip(set.iter()) {
                let n = decoder
                    .decompress_to_buffer(c, &mut back)
                    .expect("zstd decode");
                assert_eq!(&back[..n], v.as_slice());
            }
            rows.push(Row {
                coder: match level {
                    1 => "zstd-1",
                    3 => "zstd-3",
                    _ => "zstd-9",
                },
                bytes,
                raw,
                coded,
                encode_ns: encode * 1e9 / RECORDS as f64,
                decode_ns: decode * 1e9 / RECORDS as f64,
            });
        }
    }

    for r in &rows {
        let ratio = r.coded as f64 / r.raw as f64;
        // What it is worth stated in the unit the storage issues use,
        // rather than left as a ratio somebody has to multiply out.
        let per_million = (r.bytes as f64 * ratio) * 1e6 / (1 << 20) as f64;
        println!(
            "{}\t{}\t{:.3}\t{:.0}\t{:.0}\t{:.0}",
            r.coder, r.bytes, ratio, r.encode_ns, r.decode_ns, per_million
        );
    }

    // The gate, stated rather than left to be worked out from the table.
    // A cold read is a pread of a device that has not got the page, so
    // the comparison that matters is decompress against a device round
    // trip and not against the hot path.
    println!("#");
    println!("# the gate on #725: decompress must be invisible beside a cold pread");
    for r in rows.iter().filter(|r| r.coder == "zstd-3") {
        println!(
            "# {} byte record: {:.1} us to decode, so {:.2}% of a 100 us device read \
             and {:.2}% of a 10 us one",
            r.bytes,
            r.decode_ns / 1000.0,
            r.decode_ns / 1000.0,
            r.decode_ns / 100.0
        );
    }
}
