//! A long single statement write loop, for a profiler to sit on.
//!
//! The write bench reports 200 statements a shape, which is enough to
//! gate a ceiling and not enough for a sampler to say where the time
//! went. This runs one shape for as long as it is asked to, so
//! `sample` has something to count.
//!
//! Run: cargo run --release --example point_write -- delete 20000

use std::path::Path;

use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
use zu::{Config, Database};

const ROWS: u64 = 100_000;

fn build(dir: &Path, edges: &[(u32, u32)]) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join("db.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "follows", ROWS, edges).expect("load");
    let names: Vec<Vec<u8>> = (0..ROWS).map(|i| format!("seed{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let ages: Vec<u64> = (0..ROWS).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&refs)),
        ],
    )
    .expect("props");
    path
}

fn main() {
    let mut args = std::env::args().skip(1);
    let shape = args.next().unwrap_or_else(|| "delete".into());
    let writes: u64 = args
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20_000)
        .min(ROWS - 1);

    let root = std::env::temp_dir().join(format!("zu-point-write-{shape}"));
    let _ = std::fs::remove_dir_all(&root);
    let edges: Vec<(u32, u32)> = match shape.as_str() {
        "delete" => Vec::new(),
        _ => (0..ROWS as u32)
            .map(|i| (i, (i * 7 + 1) % ROWS as u32))
            .collect(),
    };
    let path = build(&root, &edges);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");

    let verb = match shape.as_str() {
        "delete" => "DELETE p",
        "detach" => "DETACH DELETE p",
        "set" => "SET p.age = p.age",
        "insert" => "",
        other => panic!("unknown shape {other}"),
    };
    let start = std::time::Instant::now();
    for i in 0..writes {
        let text = match shape.as_str() {
            "insert" => format!("INSERT (:person {{age: {i}, name: 'new'}})"),
            _ => format!("MATCH (p:person) WHERE p.age = {i} {verb}"),
        };
        conn.query(&text).expect("write");
    }
    let each = start.elapsed().as_nanos() as f64 / 1e3 / writes as f64;
    println!("{shape}: {writes} statements, {each:.0} us each");
    let _ = std::fs::remove_dir_all(&root);
}
