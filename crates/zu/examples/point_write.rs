//! A long single statement write loop, for a profiler to sit on.
//!
//! The write bench reports 200 statements a shape, which is enough to
//! gate a ceiling and not enough for a sampler to say where the time
//! went. This runs one shape for as long as it is asked to, so
//! `sample` has something to count.
//!
//! Run: cargo run --release --example point_write -- delete 20000
//!
//! `ZU_POINT_ROOT` moves the database off the temp directory, which is
//! how the fsync gets taken out of the picture: point it at a RAM disk
//! and what is left in the profile is the engine. The wall clock it
//! prints is then a wall clock on a machine with no disk, so read the
//! processor time next to it instead.

use std::path::Path;

use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
use zu::{Config, Database};

const ROWS: u64 = 100_000;

/// The tail of `struct rusage` past the two times, as bytes, because
/// nothing here reads it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Tail([u8; 112]);

impl Default for Tail {
    fn default() -> Self {
        Self([0; 112])
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rusage {
    user: [i64; 2],
    system: [i64; 2],
    tail: Tail,
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// Processor time this process has spent, user plus system, in
/// microseconds.
fn cpu_us() -> u64 {
    let mut usage = Rusage::default();
    if unsafe { getrusage(0, &mut usage) } != 0 {
        return 0;
    }
    let micros = |t: [i64; 2]| t[0] as u64 * 1_000_000 + t[1] as u64;
    micros(usage.user) + micros(usage.system)
}

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
    let asked: u64 = args.next().and_then(|n| n.parse().ok()).unwrap_or(20_000);
    // Every shape but `set1` puts the loop counter in the statement and
    // wants a row of its own, so it runs out at the table. `set1` sends
    // the same text every time and can run as long as it is asked to,
    // which is what a sampler wants.
    let writes = if shape == "set1" {
        asked
    } else {
        asked.min(ROWS - 1)
    };

    let root = std::env::var_os("ZU_POINT_ROOT")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join(format!("zu-point-write-{shape}"));
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
        "set" | "set1" => "SET p.age = p.age",
        "insert" => "",
        other => panic!("unknown shape {other}"),
    };
    let start = std::time::Instant::now();
    let cpu = cpu_us();
    for i in 0..writes {
        let text = match shape.as_str() {
            "insert" => format!("INSERT (:person {{age: {i}, name: 'new'}})"),
            // The same text every time round, so the plan cache hits
            // and what is left is the run.
            "set1" => "MATCH (p:person) WHERE p.age = 7 SET p.age = p.age".to_string(),
            _ => format!("MATCH (p:person) WHERE p.age = {i} {verb}"),
        };
        conn.query(&text).expect("write");
    }
    let spent = cpu_us() - cpu;
    let each = start.elapsed().as_nanos() as f64 / 1e3 / writes as f64;
    let each_cpu = spent as f64 / writes as f64;
    println!("{shape}: {writes} statements, {each:.0} us each, {each_cpu:.0} us cpu each");
    let _ = std::fs::remove_dir_all(&root);
}
