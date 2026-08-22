//! What a read modify write costs on a hot key, one thread and several.
//!
//! The stripe lock #591 put on the path is uncontended when the threads
//! are on different keys and is the whole cost when they are on one, so
//! both are printed: the single threaded row is what an ordinary caller
//! pays for the order, and the shared key row is what an order costs
//! when it is doing its job.

use std::sync::Arc;
use std::time::Instant;

use zu2::{Db, Durability, Options};

fn main() {
    let ops: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::create(
            &dir.path().join("rmw.zu2"),
            Options {
                durability: Durability::Async,
                ..Options::default()
            },
        )
        .expect("create"),
    );
    println!("threads  keys       ops/s      ns/op");
    for (threads, shared) in [(1usize, true), (4, true), (4, false), (8, true), (8, false)] {
        let started = Instant::now();
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let db = Arc::clone(&db);
                std::thread::spawn(move || {
                    let mut s = db.session();
                    let mut scratch = Vec::new();
                    let key = if shared {
                        b"counter".to_vec()
                    } else {
                        format!("counter{t:03}").into_bytes()
                    };
                    for _ in 0..ops / threads as u64 {
                        s.rmw(&key, &mut scratch, |current, out| {
                            let n = match current {
                                Some(b) => u64::from_le_bytes(b.try_into().expect("eight")),
                                None => 0,
                            };
                            out.extend_from_slice(&(n + 1).to_le_bytes());
                        })
                        .expect("rmw");
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().expect("worker");
        }
        let took = started.elapsed().as_secs_f64();
        println!(
            "{threads:7}  {:9}  {:9.0}  {:9.1}",
            if shared { "one" } else { "one each" },
            ops as f64 / took,
            took * 1e9 / ops as f64
        );
    }
}
