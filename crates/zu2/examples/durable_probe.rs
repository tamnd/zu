//! Where a durable commit's time goes. Not a benchmark, a probe.
//!
//! The ycsb bench answers the comparison question and takes several
//! minutes to do it, which is too slow and too noisy to tell whether one
//! change to the write path helped: on a machine at load average 25 the
//! sqlite rows move by 3x between two runs of the same binary. This runs
//! the durable path on its own, in rounds, and prints every round so the
//! spread is visible rather than averaged away.
//!
//! Both sides of the reservation question run in the same process and
//! alternate round by round, so a burst of load from something else on
//! the machine lands on both of them rather than on whichever one was
//! unlucky enough to be running. `ZU2_PROBE_ROUNDS` rounds of
//! `ZU2_PROBE_OPS` commits each, and the median round is the number to
//! quote.

use std::time::Instant;
use zu2::{Db, Durability, Options};

const LOADED: u64 = 20000;

fn env<T: std::str::FromStr>(name: &str, fallback: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn median(rates: &[f64]) -> f64 {
    let mut sorted = rates.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn report(what: &str, rates: &[f64]) {
    let each: Vec<String> = rates.iter().map(|r| format!("{r:.0}")).collect();
    println!(
        "{what:22} {:9.0} op/s median  {:8.1} us/op  rounds: {}",
        median(rates),
        1e6 / median(rates),
        each.join(" ")
    );
}

/// A database loaded with the records the rounds update, so that no
/// round is measuring an insert.
fn loaded(dir: &std::path::Path, name: &str, provision_bytes: u64) -> Db {
    let db = Db::create(
        &dir.join(name),
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 14,
            max_pages: 1 << 14,
            provision_bytes,
            ..Options::default()
        },
    )
    .expect("create");
    {
        let mut s = db.session();
        let v = vec![b'v'; 1000];
        for i in 0..LOADED {
            s.upsert(format!("user{i:019}").as_bytes(), &v)
                .expect("load");
        }
    }
    db.sync().expect("sync");
    db
}

/// One round of `n` updates, returning the rate. The key range moves
/// with the round so no round is measuring the in-place update of the
/// record the previous one just wrote.
fn round(db: &Db, durability: Durability, nth: usize, n: u64, value: &[u8]) -> f64 {
    let mut s = db.session();
    s.set_durability(durability);
    let started = Instant::now();
    for i in 0..n {
        let key = (nth as u64 * n + i) % LOADED;
        s.upsert(format!("user{key:019}").as_bytes(), value)
            .expect("update");
    }
    n as f64 / started.elapsed().as_secs_f64()
}

fn main() {
    let rounds: usize = env("ZU2_PROBE_ROUNDS", 5_usize);
    let ops: u64 = env("ZU2_PROBE_OPS", 400_u64);
    let dir = match std::env::args().nth(1) {
        Some(path) => tempfile::tempdir_in(path),
        None => tempfile::tempdir(),
    }
    .expect("tempdir");
    let chunk: u64 = env("ZU2_PROBE_CHUNK", zu2::PROVISION_CHUNK);
    println!(
        "in {}  {rounds} rounds of {ops}  reservation {} KiB",
        dir.path().display(),
        chunk / 1024
    );

    let bare = loaded(dir.path(), "bare.zu2", 0);
    let reserved = loaded(dir.path(), "reserved.zu2", chunk);
    let value = vec![b'v'; 1000];

    let mut async_rates = Vec::new();
    let mut bare_rates = Vec::new();
    let mut reserved_rates = Vec::new();
    for nth in 0..rounds {
        // Async first, as the control: it does not touch the file on
        // the commit path at all, so if it moves between rounds the
        // machine moved and the durable rows are to be read with that
        // in mind.
        async_rates.push(round(&reserved, Durability::Async, nth, ops * 20, &value));
        bare_rates.push(round(&bare, Durability::Durable, nth, ops, &value));
        reserved_rates.push(round(&reserved, Durability::Durable, nth, ops, &value));
    }
    report("async", &async_rates);
    report("durable bare", &bare_rates);
    report("durable reserved", &reserved_rates);
    println!(
        "reservation is worth {:.2}x",
        median(&reserved_rates) / median(&bare_rates)
    );
}
