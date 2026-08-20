//! What announcing the epoch once a node instead of once a walk costs a
//! traversal, which is less than the first version of this measurement
//! said.
//!
//! Every neighbour read goes out under epoch protection, because a
//! writer that grew a neighbourhood hands the old block to the epoch
//! queue rather than to the allocator, and a reader walking that block
//! has to be visible while it does. `Session::neighbours` announces on
//! the way in and stands down on the way out, which is right for one
//! read and looks wasteful for a walk: a breadth first search over a
//! frontier of n nodes pays n announcements for a window it could open
//! once.
//!
//! Two columns, and the second is the one to believe. The first is the
//! same two hop set collected twice, once with the epoch announced for
//! the whole walk and once per node, which is the comparison that
//! matters but is also swamped: the reads miss cache, the ratio moves
//! between 0.77x and 1.42x run to run on a laptop, and it was 1.42x on
//! the run that had somebody else's cargo build going. The second
//! column takes the announcement on its own, over a node with no
//! neighbours, so the loop holds the seqlock's two version loads and
//! nothing else. That one is steady at 2.2 to 4.2 ns a read, call it
//! 2.5, against a bare read's own 3.0.
//!
//! So hoisting is worth about 2.5 ns a node. A two hop probe touching
//! seventeen nodes saves forty of fourteen hundred, which is two to
//! five percent and under the noise of the first column. It is worth
//! having and it is not worth a headline, and the reason this file says
//! so at length is that the issue it came from claimed 1.42x from a
//! contaminated run.

use std::hint::black_box;

use zu2::{Db, Direction, Durability, Options};

const SIZES: [u32; 2] = [20_000, 200_000];
const DEGREE: u32 = 16;
const PROBES: u32 = 20_000;

fn main() {
    for nodes in SIZES {
        run(nodes);
    }
}

fn run(nodes: u32) {
    let node_count = nodes;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("protecting.zu2"),
        Options {
            durability: Durability::Async,
            max_nodes: 1 << 19,
            max_pages: 1 << 14,
            ..Options::default()
        },
    )
    .expect("create");
    let mut session = db.session();

    for i in 0..node_count {
        let id = session
            .add_node(format!("n{i}").as_bytes())
            .expect("add node");
        assert_eq!(id, i);
    }
    // Scattered neighbours rather than a stride set, because a stride
    // set makes the two hop neighbourhood an arithmetic progression and
    // collapses it to 2*DEGREE nodes, which measures a walk that does
    // not happen. This gives every node the same degree and a two hop
    // set near DEGREE squared.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in 0..node_count {
        for _ in 0..DEGREE {
            session
                .add_edge(i, (next() % node_count as u64) as u32)
                .expect("add edge");
        }
    }

    let mut seen = Vec::new();
    let mut first = Vec::new();
    let mut out = Vec::new();

    // Warm, so neither column is paying for the first touch of a page.
    for i in 0..PROBES {
        session.two_hop(
            Direction::Out,
            i % node_count,
            &mut seen,
            &mut first,
            &mut out,
        );
    }

    // Alternating rounds rather than one block each, because the two
    // columns differ by less than a laptop's own noise over a single
    // pass and whichever ran second would look slower. The median of
    // five interleaved rounds is what is printed.
    let mut hoisted = Vec::new();
    let mut per_node = Vec::new();
    let mut total = 0usize;
    for _ in 0..5 {
        let start = std::time::Instant::now();
        for i in 0..PROBES {
            session.two_hop(
                Direction::Out,
                i % node_count,
                &mut seen,
                &mut first,
                &mut out,
            );
            total += out.len();
        }
        hoisted.push(start.elapsed());

        let start = std::time::Instant::now();
        for i in 0..PROBES {
            two_hop_per_node(
                &mut session,
                i % node_count,
                &mut seen,
                &mut first,
                &mut out,
            );
            total += out.len();
        }
        per_node.push(start.elapsed());
    }
    black_box(total);
    hoisted.sort_unstable();
    per_node.sort_unstable();
    let hoisted = hoisted[2];
    let per_node = per_node[2];
    let total = total / 10;

    let reached = total / PROBES as usize;
    let touched = 1 + DEGREE as usize;
    println!("{node_count} nodes, degree {DEGREE}, {PROBES} probes, {reached} nodes an answer");
    println!(
        "announced once a walk  {:>8.0} probes/s  {:>6.0} ns  {:>5.1} ns a node",
        PROBES as f64 / hoisted.as_secs_f64(),
        hoisted.as_nanos() as f64 / PROBES as f64,
        hoisted.as_nanos() as f64 / (PROBES as usize * touched) as f64,
    );
    println!(
        "announced once a node  {:>8.0} probes/s  {:>6.0} ns  {:>5.1} ns a node",
        PROBES as f64 / per_node.as_secs_f64(),
        per_node.as_nanos() as f64 / PROBES as f64,
        per_node.as_nanos() as f64 / (PROBES as usize * touched) as f64,
    );
    println!(
        "per node announcement costs {:.2}x",
        per_node.as_secs_f64() / hoisted.as_secs_f64()
    );

    // The announcement on its own, with the read it wraps made as cheap
    // as a read can be: one node with no neighbours, so what is left in
    // the loop is the seqlock's two version loads and the announcement.
    // If the two hop columns cannot tell the difference, this is where
    // it either shows up or does not exist.
    let bare = node_count - 1;
    const READS: u32 = 2_000_000;
    let mut once = Vec::new();
    let mut each = Vec::new();
    for _ in 0..5 {
        let start = std::time::Instant::now();
        session.walk(|walk| {
            for _ in 0..READS {
                walk.neighbours(Direction::Out, bare, |slice| black_box(slice.len()));
            }
        });
        once.push(start.elapsed());

        let start = std::time::Instant::now();
        for _ in 0..READS {
            session.neighbours(Direction::Out, bare, |slice| black_box(slice.len()));
        }
        each.push(start.elapsed());
    }
    once.sort_unstable();
    each.sort_unstable();
    let (once, each) = (once[2], each[2]);
    println!(
        "bare read, announced once  {:>6.1} ns   announced per read  {:>6.1} ns   difference {:>5.1} ns",
        once.as_nanos() as f64 / READS as f64,
        each.as_nanos() as f64 / READS as f64,
        (each.as_nanos() as f64 - once.as_nanos() as f64) / READS as f64,
    );
}

/// The same two hop set as `Session::two_hop`, built out of the public
/// per read entry point, which is what every walk in zu2-capi does.
fn two_hop_per_node(
    session: &mut zu2::Session<'_>,
    node: u32,
    seen: &mut Vec<u64>,
    first: &mut Vec<u32>,
    out: &mut Vec<u32>,
) {
    let words = (session.core_ref().graph().nodes() as usize)
        .div_ceil(64)
        .max(1);
    if seen.len() < words {
        seen.resize(words, 0);
    }
    out.clear();
    session.neighbours_into(Direction::Out, node, first);
    let mut collected = std::mem::take(out);
    for &near in first.iter() {
        session.neighbours(Direction::Out, near, |slice| {
            for &far in slice {
                let word = far as usize / 64;
                let bit = 1u64 << (far % 64);
                if word < seen.len() && seen[word] & bit == 0 {
                    seen[word] |= bit;
                    collected.push(far);
                }
            }
        });
    }
    for &far in collected.iter() {
        seen[far as usize / 64] &= !(1u64 << (far % 64));
    }
    *out = collected;
}
