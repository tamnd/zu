//! A persistent worker pool for the morsel scheduler.
//!
//! The executor used to spawn scoped threads per query, which priced
//! parallelism at eight thread creations per statement. Linux absorbs
//! that; Windows charges around a quarter millisecond per thread and
//! the parallel path lost to the sequential one on queries under a
//! few milliseconds. Threads here spawn on first use, park between
//! queries, and pick jobs off a shared queue, so a warm query pays a
//! mutex push and an unpark.
//!
//! Jobs borrow the caller's stack. That is sound because
//! [`submit`] hands back a [`Pending`] whose wait (or drop) blocks
//! until every submitted job has finished, so the borrows outlive
//! every use; the transmute to `'static` never outlives the call.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
    /// Workers alive, capped at the host's parallelism; a submit that
    /// finds every worker busy and the cap unreached spawns one more.
    workers: AtomicUsize,
    idle: AtomicUsize,
}

fn queue() -> &'static Queue {
    static QUEUE: OnceLock<Queue> = OnceLock::new();
    QUEUE.get_or_init(|| Queue {
        jobs: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        workers: AtomicUsize::new(0),
        idle: AtomicUsize::new(0),
    })
}

fn worker_loop(q: &'static Queue) {
    loop {
        let job = {
            let mut jobs = q.jobs.lock().unwrap();
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                q.idle.fetch_add(1, Ordering::Relaxed);
                jobs = q.ready.wait(jobs).unwrap();
                q.idle.fetch_sub(1, Ordering::Relaxed);
            }
        };
        // A worker outlives any one job's panic: the unwind stops here
        // so the thread keeps serving and the worker count stays true.
        // The panicking query reports the failure through its empty
        // result slot.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}

/// Completion latch for one batch of submitted jobs. Waiting is
/// mandatory; dropping it waits too, so a panic on the submitting
/// thread still keeps borrowed jobs alive until they finish.
pub(crate) struct Pending {
    latch: Arc<(Mutex<usize>, Condvar)>,
}

impl Pending {
    pub(crate) fn wait(&self) {
        let (left, cv) = &*self.latch;
        let mut left = left.lock().unwrap();
        while *left > 0 {
            left = cv.wait(left).unwrap();
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.wait();
    }
}

/// Runs `jobs` on the pool and returns the latch that fences their
/// borrows. Each job runs exactly once, panics included: a panicking
/// job counts down on unwind so the latch cannot hang, and the caller
/// sees the panic as its result slot staying empty.
pub(crate) fn submit<'a>(jobs: Vec<Box<dyn FnOnce() + Send + 'a>>) -> Pending {
    let q = queue();
    let latch = Arc::new((Mutex::new(jobs.len()), Condvar::new()));
    let cap = std::thread::available_parallelism().map_or(1, |n| n.get());
    // SAFETY: Pending waits, in wait() or in drop, until every job has
    // run, so the 'a borrows inside the jobs are live for as long as
    // any worker can touch them.
    let jobs: Vec<Job> = unsafe { std::mem::transmute(jobs) };
    for job in jobs {
        let latch = Arc::clone(&latch);
        let wrapped: Job = Box::new(move || {
            struct CountDown(Arc<(Mutex<usize>, Condvar)>);
            impl Drop for CountDown {
                fn drop(&mut self) {
                    let (left, cv) = &*self.0;
                    *left.lock().unwrap() -= 1;
                    cv.notify_all();
                }
            }
            let _count = CountDown(latch);
            job();
        });
        let mut jobs = q.jobs.lock().unwrap();
        jobs.push_back(wrapped);
        if q.idle.load(Ordering::Relaxed) == 0 && q.workers.load(Ordering::Relaxed) < cap {
            q.workers.fetch_add(1, Ordering::Relaxed);
            std::thread::Builder::new()
                .name("zu-exec-worker".into())
                .spawn(move || worker_loop(q))
                .expect("spawn a pool worker");
        }
        drop(jobs);
        q.ready.notify_one();
    }
    Pending { latch }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_run_and_borrows_survive() {
        let results: Vec<Mutex<u64>> = (0..16).map(|_| Mutex::new(0)).collect();
        let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = results
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                Box::new(move || *slot.lock().unwrap() = i as u64 + 1)
                    as Box<dyn FnOnce() + Send + '_>
            })
            .collect();
        submit(jobs).wait();
        for (i, slot) in results.iter().enumerate() {
            assert_eq!(*slot.lock().unwrap(), i as u64 + 1);
        }
    }

    #[test]
    fn a_panicking_job_still_counts_down() {
        let ok = Mutex::new(false);
        let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = vec![
            Box::new(|| panic!("job panic reaches nobody")),
            Box::new(|| *ok.lock().unwrap() = true),
        ];
        submit(jobs).wait();
        assert!(*ok.lock().unwrap());
    }
}
