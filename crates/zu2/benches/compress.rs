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

    end_to_end();
}

/// The same question asked of the engine rather than of the coder.
///
/// The table above is zstd on a buffer, which is the ceiling on what
/// this can save and the floor on what it can cost. What a host sees is
/// a point read of a record the tier holds, and that is a `pread`, a
/// checksum, a decompress and a copy. So this builds the same database
/// twice, once with the coder and once without, and reports the file
/// and the read side by side. If the file is smaller and the read is
/// not slower by a share anyone can see, #725 is paid for.
fn end_to_end() {
    println!("#");
    println!("# the same thing end to end, a database at a time (#725)");
    println!("setting	cold_MiB	ratio	read_us	span_MiB");
    let mut before = 0.0;
    for compress in [false, true] {
        let (disk, ratio, read, span) = tier(compress);
        println!(
            "{}	{:.1}	{:.4}	{:.2}	{:.1}",
            if compress { "zstd-3" } else { "plain" },
            disk as f64 / (1 << 20) as f64,
            ratio,
            read * 1e6,
            span as f64 / (1 << 20) as f64
        );
        if !compress {
            before = read;
        } else {
            println!(
                "# the coder costs {:+.1}% on a cold read and saves {:.1}% of the file",
                100.0 * (read - before) / before,
                100.0 * (1.0 - ratio)
            );
        }
    }
}

/// Loads a cold tier and times reads out of it, returning the bytes it
/// costs the device, what the coder saved, the mean read, and the span.
///
/// The shape is `tests/coldtier.rs`'s: a record is only cold by having
/// survived a lap of the log unwritten, so the load is followed by a
/// churn over a small hot set until the log has lapped. Promotion is
/// off, because a promoted record is read out of the log the second time
/// and this is measuring the tier.
fn tier(compress: bool) -> (u64, f64, f64, u64) {
    use zu2::{Db, Durability, Options};

    const RECORDS: u32 = 20_000;
    const BYTES: usize = 1000;

    let dir = tempfile::tempdir().expect("tempdir");
    let options = Options {
        durability: Durability::Async,
        index_buckets: 1 << 16,
        max_pages: 64,
        max_nodes: 1 << 18,
        mutable_pages: 1,
        compact_below: 0,
        promote_reads: false,
        compress_cold: compress,
        ..Options::default()
    };
    let db = Db::create(&dir.path().join("z.zu2"), options).expect("create");
    let set = values(RECORDS as usize, BYTES, 0x243F6A8885A308D3);
    let churn = values(2000, BYTES, 0x13198A2E03707344);

    let mut s = db.session();
    for (i, v) in set.iter().enumerate() {
        s.upsert(format!("user{i:09}").as_bytes(), v).expect("load");
    }
    // Enough laps that the loaded set is below the boundary and a pass
    // can reach it, and a hot set small enough that the laps are cheap.
    for round in 0..40u32 {
        for (i, v) in churn.iter().enumerate() {
            s.upsert(format!("hot_{round}_{i:09}").as_bytes(), v)
                .expect("churn");
        }
    }
    s.set_durability(Durability::Durable);
    s.upsert(b"hot_last", &churn[0]).expect("churn");
    drop(s);
    while db.compact().expect("compact") > 0 {}

    let mut s = db.session();
    let mut out = Vec::with_capacity(BYTES + 64);
    let mut read = f64::MAX;
    for _ in 0..REPEATS {
        let t = Instant::now();
        let mut held = 0usize;
        for i in 0..RECORDS as usize {
            out.clear();
            if s.read(format!("user{i:09}").as_bytes(), &mut out)
                .expect("read")
            {
                held += out.len();
            }
        }
        std::hint::black_box(held);
        read = read.min(t.elapsed().as_secs_f64() / RECORDS as f64);
    }
    drop(s);

    let (given, stored) = db.cold_value_bytes();
    let ratio = if given > 0 {
        stored as f64 / given as f64
    } else {
        1.0
    };
    (
        db.cold_disk_bytes().expect("cold disk bytes"),
        ratio,
        read,
        db.cold_span(),
    )
}
