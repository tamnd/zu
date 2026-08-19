//! Where a durable commit's time goes. Not a benchmark, a probe.
use std::time::Instant;
use zu2::{Db, Durability, Options};

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("p.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 14,
            max_pages: 1 << 14,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let v = vec![b'v'; 1000];
    for i in 0..20000u64 {
        s.upsert(format!("user{i:019}").as_bytes(), &v)
            .expect("load");
    }
    for (what, d) in [
        ("async", Durability::Async),
        ("durable", Durability::Durable),
    ] {
        s.set_durability(d);
        let n = if d == Durability::Async { 20000 } else { 2000 };
        let t = Instant::now();
        for i in 0..n {
            s.upsert(format!("user{i:019}").as_bytes(), &v)
                .expect("update");
        }
        let e = t.elapsed().as_secs_f64();
        println!(
            "{what:8} {:9.0} op/s  {:8.1} us/op",
            n as f64 / e,
            e / n as f64 * 1e6
        );
    }
}
