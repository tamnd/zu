//! What a horizontal aggregate costs (ISO 20.9, feature GE09).
//!
//! An aggregate written around a group variable folds what one row
//! bound rather than what the rows held, so `SUM(y.id)` over a stretch
//! repeated eight times adds eight numbers and answers one, on every
//! row, and the rows are not grouped at all. The plan/04 design note
//! asked for that to be a kernel over the group's elements rather than
//! a loop over a list built for the purpose, and these are the numbers
//! that say which of the two it is.
//!
//! Three shapes over one graph and one pattern. The first is the match
//! with neither the fold nor the list on it, which is what the walk
//! costs and what both of the others pay before they do anything. The
//! second is the fold. The third is the same group read without an
//! aggregate around it, which is the list of the same elements, and it
//! is the honest thing to price the fold against: an implementation
//! that folded by looping would build that list first and then walk it,
//! so the list on its own is the cheaper half of the way this is not
//! done.
//!
//! The measurement that decides it is the allocator and not the clock.
//! A fold that built the list it folds would ask for it, once a row, so
//! the bytes each shape asks the allocator for is the question put
//! directly: the fold asks for nothing over the walk and the list asks
//! for 1024 bytes a row, on any machine at any load, and
//! fold_alloc_bytes_row is the gate on that.
//!
//! The clock is reported too and gated loosely. The walk is nine tenths
//! of every shape's time, so the aggregation is a difference of two
//! large numbers and moves with the machine: it reads between a half
//! and about one and a tenth of the list build over repeated runs on a
//! busy machine, where a fold that built its list would read near one
//! and seven tenths. fold_over_list_x is set to catch that and not to
//! catch a tenth.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench fold

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

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

/// Bytes handed out while the counter was on, and whether it is on.
///
/// A time can be argued with and this cannot: a fold that built the
/// list it folds would ask the allocator for it, once a row, and the
/// number below would say so on any machine at any load. The counter is
/// off except during the counted runs, so the timed rounds pay a
/// relaxed load per allocation and nothing else, and all three shapes
/// pay it alike.
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    /// A grown allocation counts what it grew by, so a list that
    /// doubles its way to a size counts that size and not several
    /// multiples of it. The call goes to the system realloc rather than
    /// through the default alloc and copy, since the point is to leave
    /// the shapes running the way they run.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) && new_size > layout.size() {
            ALLOCATED.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The bytes one run of `source` asks the allocator for.
fn allocated(session: &mut Session, source: &str, want: usize) -> u64 {
    ALLOCATED.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let answer = session.run(source, &[]).expect("counted run");
    COUNTING.store(false, Ordering::Relaxed);
    let bytes = ALLOCATED.load(Ordering::Relaxed);
    assert_eq!(answer.rows.len(), want, "row count for {source}");
    bytes
}

/// The tail of `struct rusage` this bench does not read, sized so the
/// kernel writes inside the allocation rather than past it.
const RUSAGE_TAIL: usize = 14;

/// As much of `struct rusage` as the peak resident size needs. The two
/// timevals in front of that field are two words each on both platforms
/// this runs on, so it lands at the same offset on either.
#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    tail: [i64; RUSAGE_TAIL],
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// The processor time this process has spent, in microseconds, user
/// and system together.
fn cpu_us() -> u64 {
    let mut usage = Rusage::default();
    // RUSAGE_SELF is 0 on every platform that has the call.
    if unsafe { getrusage(0, &mut usage) } != 0 {
        return 0;
    }
    let micros = |t: [i64; 2]| (t[0].max(0) as u64) * 1_000_000 + t[1].max(0) as u64;
    micros(usage.utime) + micros(usage.stime)
}

#[cfg(target_os = "macos")]
mod platform {
    /// `struct rusage_info_v0` read as the words it is: sixteen bytes of
    /// uuid and then u64 fields, with room past the one this reads so
    /// the kernel fills the buffer rather than the stack behind it.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Info {
        uuid: [u8; 16],
        words: [u64; 32],
    }

    /// Resident size is the seventh word of `rusage_info_v0`.
    const RESIDENT: usize = 6;
    const FLAVOR: i32 = 0;

    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buf: *mut Info) -> i32;
    }

    /// Resident bytes, or zero if the call refuses.
    pub(super) fn rss() -> u64 {
        let mut info = Info::default();
        if unsafe { proc_pid_rusage(std::process::id() as i32, FLAVOR, &mut info) } != 0 {
            return 0;
        }
        info.words[RESIDENT]
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// Resident bytes from `statm`, whose second field is resident pages.
    pub(super) fn rss() -> u64 {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096)
            .unwrap_or(0)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    pub(super) fn rss() -> u64 {
        0
    }
}

/// A chain, so every node but the last few starts a walk of the length
/// the quantifier asks for and every walk is the same size. Fifty
/// thousand of them is enough that the per row cost is what is being
/// measured rather than the statement being prepared, and small enough
/// that nine rounds of three shapes at one worker finish in a minute.
const NODES: u64 = 50_000;
/// Repetitions, which is how many elements one row's group holds.
const REPEAT: u64 = 8;
/// Timed rounds, one run of each shape to a round.
const RUNS: usize = 9;

fn rows() -> usize {
    (NODES - REPEAT) as usize
}

/// What a shape cost: every round's wall clock, the processor time one
/// run burned, and the resident bytes the answer takes to hold,
/// measured with the answer in hand against the same process with it
/// dropped.
struct Cost {
    cpu_us: u64,
    held: u64,
    bytes: u64,
    runs: Vec<f64>,
}

impl Cost {
    /// The least round, which is the right statistic for a shape
    /// measured on its own: everything above it is interference.
    fn least(&self) -> f64 {
        self.runs.iter().copied().fold(f64::MAX, f64::min)
    }
}

/// The middle of a list of numbers, which is the statistic the
/// differences and the ratio are taken at.
///
/// A difference between two shapes is not a shape and does not want the
/// least of anything: a round where the walk came out low and the fold
/// came out high is as far wrong as the other way round, and the least
/// of each side picks exactly that pair. The median of the rounds
/// throws both away, and it throws away a round where the difference
/// came out negative or near zero as well, which is what a loaded
/// machine produces and what would otherwise read as an enormous ratio.
fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

/// What the answer of one run takes to hold, and one run's worth of
/// processor time, before the rounds start.
fn held_and_cpu(session: &mut Session, source: &str, want: usize) -> (u64, u64) {
    let warm = session.run(source, &[]).expect("warm run");
    assert_eq!(warm.rows.len(), want, "row count for {source}");
    drop(warm);
    // The answer once, held, against the same process without it. The
    // statement has run before, so what moves between these two
    // readings is the rows and not the buffers reading them filled.
    let empty = platform::rss();
    let answer = session.run(source, &[]).expect("held run");
    let held = platform::rss().saturating_sub(empty);
    drop(answer);
    let before = cpu_us();
    session.run(source, &[]).expect("cpu run");
    (held, cpu_us().saturating_sub(before))
}

fn time(session: &mut Session, source: &str, want: usize) -> f64 {
    let t = Instant::now();
    let r = session.run(source, &[]).expect("timed run");
    let us = t.elapsed().as_secs_f64() * 1e6;
    assert_eq!(r.rows.len(), want, "row count for {source}");
    us
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fold.zu1");
    let edges: Vec<(u32, u32)> = (0..NODES as u32 - 1).map(|i| (i, i + 1)).collect();
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
    drop(db);
    // One worker, because the number that matters is a difference
    // between two shapes and a thread pool moves that around by more
    // than the difference is.
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    let mut session = Session::open(&path).expect("session");

    let pattern = "MATCH (a:person)((x:person)-[:knows]->(y:person)){8}(b:person)";
    let walk = format!("{pattern} RETURN b.id AS n");
    let fold = format!("{pattern} RETURN SUM(y.id) AS total");
    let list = format!("{pattern} RETURN y.id AS steps");

    // The fold answers the sum of the eight nodes the walk reached, and
    // for the walk that starts at node k those are k+1 to k+8. A run
    // that got fast by folding the wrong elements fails here.
    let first = session
        .run(
            &format!("{pattern} WHERE a.id = 0 RETURN SUM(y.id) AS total"),
            &[],
        )
        .expect("crosscheck");
    let want: i64 = (1..=REPEAT as i64).sum();
    assert_eq!(
        first.rows.first().and_then(|row| row.first()),
        Some(&Value::Int(want)),
        "the fold is the sum of the elements the walk reached"
    );

    let shapes = [("walk only", &walk), ("fold", &fold), ("list", &list)];
    let mut costs: Vec<Cost> = shapes
        .iter()
        .map(|(_, source)| {
            let (held, cpu_us) = held_and_cpu(&mut session, source, rows());
            Cost {
                cpu_us,
                held,
                bytes: allocated(&mut session, source, rows()),
                runs: Vec::with_capacity(RUNS),
            }
        })
        .collect();
    // Round robin rather than one shape at a time, so a machine that
    // slows down halfway through slows all three of them and not the
    // one that happened to be running.
    for _ in 0..RUNS {
        for (cost, (_, source)) in costs.iter_mut().zip(shapes.iter()) {
            let us = time(&mut session, source, rows());
            cost.runs.push(us);
        }
    }
    let (walk_cost, fold_cost, list_cost) = (&costs[0], &costs[1], &costs[2]);

    // Round by round, so a difference is taken between two runs that
    // met the same machine.
    let mut fold_owns = Vec::with_capacity(RUNS);
    let mut list_owns = Vec::with_capacity(RUNS);
    let mut ratios = Vec::with_capacity(RUNS);
    for round in 0..RUNS {
        let (w, f, l) = (
            walk_cost.runs[round],
            fold_cost.runs[round],
            list_cost.runs[round],
        );
        fold_owns.push(f - w);
        list_owns.push(l - w);
        ratios.push((f - w) / (l - w));
    }
    let fold_own = median(&fold_owns);
    let list_own = median(&list_owns);
    let ratio = median(&ratios);
    let elements = rows() as f64 * REPEAT as f64;
    let per_element = fold_own * 1e3 / elements;

    println!(
        "fold: {NODES} nodes chained, {} rows, {REPEAT} elements a row, {elements:.0} elements folded",
        rows()
    );
    for ((what, _), cost) in shapes.iter().zip(costs.iter()) {
        println!(
            "fold {what}: {:.0} us least, {:.0} us median, {} us cpu, \
             {:.1} MiB of answer held, {:.1} bytes a row allocated",
            cost.least(),
            median(&cost.runs),
            cost.cpu_us,
            cost.held as f64 / 1048576.0,
            cost.bytes as f64 / rows() as f64,
        );
    }
    let per_row = |cost: &Cost| cost.bytes as f64 / rows() as f64;
    let fold_bytes = per_row(fold_cost) - per_row(walk_cost);
    let list_bytes = per_row(list_cost) - per_row(walk_cost);
    println!(
        "fold aggregation: {fold_own:.0} us, {per_element:.1} ns an element, {fold_bytes:.1} bytes a row over the walk"
    );
    println!("fold list build: {list_own:.0} us, {list_bytes:.1} bytes a row over the walk");
    println!("fold over list: {ratio:.3}x over {RUNS} rounds, crosschecked");

    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        if let Some(ceiling) = budget("fold_group_us") {
            assert!(
                fold_cost.least() <= ceiling,
                "the fold took {:.0} us, over the {ceiling} us ceiling",
                fold_cost.least()
            );
        }
        if let Some(ceiling) = budget("fold_alloc_bytes_row") {
            assert!(
                fold_bytes <= ceiling,
                "the fold asked the allocator for {fold_bytes:.1} bytes a row more than \
                 the same match with no fold on it, over the {ceiling} byte ceiling"
            );
        }
        if let Some(ceiling) = budget("fold_over_list_x") {
            assert!(
                ratio <= ceiling,
                "the fold cost {ratio:.3}x the list it would otherwise build, \
                 over the {ceiling}x ceiling"
            );
        }
        println!("gate: horizontal aggregate ceilings met");
    }
}
