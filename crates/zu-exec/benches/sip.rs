//! P3 SIP gate (Spec/2064g/perf/13 section 1, zu#76): the filter a join
//! build publishes, measured on the two shapes a build side comes in
//! and against the join table's own directory tags.
//!
//! Two build sides. The dense one is node ids, which is what a graph
//! join builds over and what the exact bitmap is for. The scattered one
//! is keys spread over the whole word, which no bitmap fits, so it gets
//! the bloom. Both hold the same number of keys, so the difference
//! between them is the filter and not the size of the problem.
//!
//! The probe side is one in sixteen a match, which is the shape SIP is
//! for: a filter that keeps everything saves nothing, and the interest
//! is in how cheaply the other fifteen sixteenths can be dropped. The
//! misses sit inside the build side's range on purpose, so the range
//! check alone cannot answer them and the membership test has to.
//!
//! Every run is crosschecked. A filter that drops a real match fails
//! here, and the false positive rate is printed rather than assumed.
//!
//! The end-to-end pair is the reason the filter exists. Downstream of a
//! filter a surviving row costs a property gather, a random read into a
//! column, and then the join. Gathering every row and joining every row
//! is what the engine does today. Selecting first and gathering only
//! the survivors is what SIP buys, and the gap is what the gate cares
//! about.
//!
//! exec_sip_select_mrows_s floors the bloom select, the general case
//! and the slower of the two filters.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-exec --bench sip

use std::hint::black_box;
use std::time::Instant;

use zu_exec::join::JoinTable;
use zu_exec::sip::SipFilter;

fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    for line in std::fs::read_to_string(path).ok()?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return v.trim().parse().ok();
        }
    }
    None
}

const BUILD: u64 = 1_000_000;
const PROBE: u64 = 4_000_000;
/// One probe row in this many is a real match.
const HIT_IN: u64 = 16;
/// The property column a surviving row gathers from, in entries. 32 MB,
/// large enough that a gather is a real miss rather than a cache read.
const PROPS: usize = 4 << 20;

/// The dense build side: node ids, every third one, so the misses have
/// somewhere to sit inside the same span.
fn dense_key(i: u64) -> u64 {
    i * 3 + 7
}

/// A dense id inside the build side's span that is never a build key.
fn dense_miss(i: u64) -> u64 {
    (i % (BUILD - 1)) * 3 + 8
}

/// The scattered build side: keys strided by a large odd number, so
/// they cover the whole word and no bitmap can hold them.
fn wide_key(i: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// True for the probe rows that are meant to match, spread over the
/// probe side rather than clustered, so neither the predictor nor the
/// prefetcher learns the pattern.
fn is_hit(i: u64) -> bool {
    i.wrapping_mul(0xD6E8_FEB8_6659_FD93) >> 60 < 16 / HIT_IN
}

/// The p50 of `runs` timings of `f`, in milliseconds.
fn measure(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut ms: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}

fn main() {
    let dense: Vec<u64> = (0..BUILD).map(dense_key).collect();
    let wide: Vec<u64> = (0..BUILD).map(wide_key).collect();

    let dense_ms = measure(5, || {
        black_box(SipFilter::over(&dense));
    });
    let wide_ms = measure(5, || {
        black_box(SipFilter::over(&wide));
    });
    let dmask = SipFilter::over(&dense);
    let wbloom = SipFilter::over(&wide);
    assert_eq!(dmask.kind(), "mask", "dense ids should take the bitmap");
    assert_eq!(
        wbloom.kind(),
        "bloom",
        "scattered keys should take the bloom"
    );

    // The join table over the same scattered keys, so its directory
    // tags answer the same question the bloom does and the comparison
    // is a comparison and not two different problems.
    let payload: Vec<u64> = (0..BUILD).collect();
    let table = JoinTable::build(&wide, &payload);

    println!(
        "sip: mask built in {dense_ms:.2} ms, {:.2} MB over {} keys",
        dmask.bytes() as f64 / 1e6,
        dmask.keys()
    );
    println!(
        "sip: bloom built in {wide_ms:.2} ms, {:.2} MB over {} keys",
        wbloom.bytes() as f64 / 1e6,
        wbloom.keys()
    );
    println!(
        "sip: the join table the tags live in is {:.2} MB",
        table.bytes() as f64 / 1e6
    );

    // Probe sides. One in HIT_IN is a real match, the rest miss inside
    // the build side's own range.
    let dprobe: Vec<u64> = (0..PROBE)
        .map(|i| {
            if is_hit(i) {
                dense_key(i % BUILD)
            } else {
                dense_miss(i)
            }
        })
        .collect();
    let wprobe: Vec<u64> = (0..PROBE)
        .map(|i| {
            if is_hit(i) {
                wide_key(i % BUILD)
            } else {
                wide_key(i + BUILD)
            }
        })
        .collect();
    let want_hits = (0..PROBE).filter(|i| is_hit(*i)).count();
    assert!(want_hits > 0, "a probe side with no matches proves nothing");

    let mut sel = vec![0u32; PROBE as usize];

    let mut kept = 0usize;
    let mask_ms = measure(5, || {
        kept = dmask.select(&dprobe, &mut sel);
    });
    // The mask is exact, so this is not a rate, it is a correctness
    // check with a number attached.
    assert_eq!(kept, want_hits, "the exact filter lost or invented rows");
    report("mask select", mask_ms, kept, want_hits);

    let bloom_ms = measure(5, || {
        kept = wbloom.select(&wprobe, &mut sel);
    });
    assert!(kept >= want_hits, "the bloom lost a real match");
    report("bloom select", bloom_ms, kept, want_hits);
    let bloom_kept = kept;

    // The same selection off the join table's directory tags, which is
    // the filter the table already has and the one perf/13 suggests
    // reusing. Written out here rather than wrapped in a SipFilter,
    // since the point of measuring it is to decide whether it should be
    // one at all.
    let tag_ms = measure(5, || {
        let mut n = 0;
        for (i, k) in wprobe.iter().enumerate() {
            sel[n] = i as u32;
            n += usize::from(table.may_contain(*k));
        }
        kept = n;
    });
    assert!(kept >= want_hits, "the tags lost a real match");
    report("tag select", tag_ms, kept, want_hits);

    // End to end. Downstream of the filter a row costs a property
    // gather and a join probe, and the question is whether the fifteen
    // sixteenths that will not survive should pay for either.
    let props: Vec<u64> = (0..PROPS as u64).collect();
    let mut sum = 0u64;
    let mut pairs = 0usize;
    let all_ms = measure(5, || {
        let mut s = 0u64;
        let mut p = 0usize;
        for k in &wprobe {
            s = s.wrapping_add(props[(*k as usize) & (PROPS - 1)]);
            for row in table.lookup(*k) {
                p += 1;
                s = s.wrapping_add(*row);
            }
        }
        sum = s;
        pairs = p;
    });
    let want_sum = sum;
    let want_pairs = pairs;

    let sip_ms = measure(5, || {
        let n = wbloom.select(&wprobe, &mut sel);
        let mut s = 0u64;
        let mut p = 0usize;
        for at in &sel[..n] {
            let k = wprobe[*at as usize];
            s = s.wrapping_add(props[(k as usize) & (PROPS - 1)]);
            for row in table.lookup(k) {
                p += 1;
                s = s.wrapping_add(*row);
            }
        }
        pairs = p;
        // The rows the filter dropped gathered nothing, so their
        // property values are missing from this sum. Add them back the
        // cheap way, off the values the gather would have read, so the
        // two paths are checked against each other and not just timed.
        sum = s;
    });
    assert_eq!(pairs, want_pairs, "the filtered join lost pairs");
    let dropped: u64 = wprobe
        .iter()
        .filter(|k| !wbloom.may_contain(**k))
        .map(|k| props[(*k as usize) & (PROPS - 1)])
        .fold(0u64, |a, b| a.wrapping_add(b));
    assert_eq!(
        sum.wrapping_add(dropped),
        want_sum,
        "the two paths read different properties"
    );
    println!(
        "sip: gather every row then join {all_ms:.2} ms, select then gather {sip_ms:.2} ms ({:.1}x)",
        all_ms / sip_ms
    );
    println!(
        "sip: the filter passed {bloom_kept} of {PROBE} rows to that gather, {want_pairs} of them joined"
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_sip_select_mrows_s")
    {
        let rate = PROBE as f64 / 1e3 / bloom_ms;
        assert!(
            rate >= floor,
            "bloom select at {rate:.0} M rows/s under the {floor} M floor"
        );
        println!("gate: sip select floor met");
    }
}

/// One select line: the rate it ran at and how much of what it passed
/// was not really a match.
fn report(what: &str, ms: f64, kept: usize, want: usize) {
    let fp = (kept - want) as f64 * 100.0 / (PROBE as usize - want) as f64;
    println!(
        "sip: {what} {ms:.2} ms, {:.0} M rows/s, kept {kept} of {PROBE}, {fp:.2}% false positive",
        PROBE as f64 / 1e3 / ms
    );
}
