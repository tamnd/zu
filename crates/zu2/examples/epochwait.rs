//! What the two ways of asking "is everything below me complete?" cost.
//!
//! A durable commit has to know that no session is still in the middle
//! of writing a record below the address it is about to hand to the
//! device. Waiting for the epoch to turn over answers that by waiting
//! for every session to leave whatever it was doing, readers included.
//! Reading the published write frontiers answers it by looking, and a
//! session that is only reading does not appear.
//!
//! This measures the two on their own, with no log and no device
//! underneath, because inside a commit they sit next to an `fsync` and
//! anything the device does swamps them. Sessions here alternate
//! between protected and idle at whatever rate the machine allows,
//! which is the worst case for the first one and no case at all for the
//! second.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use zu2::epoch::{Epochs, Slotted};

fn env<T: std::str::FromStr>(name: &str, fallback: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn main() {
    let calls: u64 = env("ZU2_PROBE_CALLS", 20000_u64);
    let counts: Vec<usize> = env::<String>("ZU2_PROBE_SESSIONS", "0 1 2 4 8".to_string())
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    println!("{calls} calls each, {cores} cores");
    println!(
        "{:>8}{:>16}{:>16}{:>10}",
        "readers", "quiescence", "frontier", "ratio"
    );

    for readers in counts {
        let epochs = Epochs::new(64);
        let stop = AtomicBool::new(false);
        let ready = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..readers {
                scope.spawn(|| {
                    let session = Slotted::new(&epochs);
                    ready.fetch_add(1, Ordering::Release);
                    while !stop.load(Ordering::Relaxed) {
                        // A reader announcing and standing down as fast
                        // as it can, which is the shape of a session
                        // doing small reads out of memory.
                        session.protect();
                        std::hint::spin_loop();
                        session.unprotect();
                    }
                });
            }
            while ready.load(Ordering::Acquire) < readers as u64 {
                std::hint::spin_loop();
            }

            let started = Instant::now();
            for _ in 0..calls {
                epochs.wait_for_quiescence();
            }
            let quiescence = started.elapsed().as_secs_f64() / calls as f64 * 1e9;

            let started = Instant::now();
            let mut floor = 0u64;
            for i in 0..calls {
                // The ceiling stands in for the tail, and nothing here
                // is writing, so this is the cost of the look itself.
                floor = floor.wrapping_add(epochs.write_floor(i));
            }
            let frontier = started.elapsed().as_secs_f64() / calls as f64 * 1e9;
            std::hint::black_box(floor);

            stop.store(true, Ordering::Release);
            println!(
                "{readers:>8}{quiescence:>13.0} ns{frontier:>13.0} ns{:>10.1}x",
                quiescence / frontier
            );
        });
    }
}
