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

/// How long a worker that ran out of jobs spins on the pending
/// counter before parking on the condvar. Windows takes anywhere from
/// half a millisecond to four to wake a parked thread, which is
/// longer than most queries, so parked workers were missing whole
/// queries on gamingpc; a spinning worker picks a job up in
/// microseconds and back-to-back queries stay inside the window.
#[cfg(windows)]
const SPIN: std::time::Duration = std::time::Duration::from_millis(1);

/// Elsewhere a parked thread wakes in microseconds and spinning only
/// steals cores from the query still running, so workers park at once.
#[cfg(not(windows))]
const SPIN: std::time::Duration = std::time::Duration::ZERO;

/// How long the submitting thread spins on the latch before parking.
///
/// It is not a worker and the argument above does not apply to it: it
/// has just finished its own share and the hands it is waiting on are
/// inside microseconds of finishing theirs, so the core it holds is
/// one nobody else wants for that stretch. Parking instead costs a
/// wakeup, which timed in place on a quiet 32 core box came to more
/// than a tenth of a millisecond against a query that runs in one.
#[cfg(not(windows))]
const LATCH_SPIN: std::time::Duration = std::time::Duration::from_micros(200);

/// Windows already spins a whole millisecond everywhere else in here,
/// for the same reason and at four times the cost, so the latch keeps
/// the number it has always used.
#[cfg(windows)]
const LATCH_SPIN: std::time::Duration = SPIN;

struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
    /// Workers alive, capped at the host's parallelism; a submit that
    /// finds every worker busy and the cap unreached spawns one more.
    workers: AtomicUsize,
    /// Workers spinning or parked, all of them about to pick up the
    /// next job, so submit does not have to spawn past them.
    idle: AtomicUsize,
    /// Queued jobs, mirrored outside the mutex so spinning workers
    /// poll one relaxed load instead of hammering the lock.
    pending: AtomicUsize,
}

fn queue() -> &'static Queue {
    static QUEUE: OnceLock<Queue> = OnceLock::new();
    QUEUE.get_or_init(|| Queue {
        jobs: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        workers: AtomicUsize::new(0),
        idle: AtomicUsize::new(0),
        pending: AtomicUsize::new(0),
    })
}

fn pop(q: &Queue) -> Option<Job> {
    let job = q.jobs.lock().unwrap().pop_front();
    if job.is_some() {
        q.pending.fetch_sub(1, Ordering::Relaxed);
    }
    job
}

fn worker_loop(q: &'static Queue) {
    loop {
        let job = 'get: {
            if let Some(job) = pop(q) {
                break 'get job;
            }
            q.idle.fetch_add(1, Ordering::Relaxed);
            let start = std::time::Instant::now();
            while start.elapsed() < SPIN {
                if q.pending.load(Ordering::Relaxed) > 0
                    && let Some(job) = pop(q)
                {
                    q.idle.fetch_sub(1, Ordering::Relaxed);
                    break 'get job;
                }
                std::hint::spin_loop();
            }
            let mut jobs = q.jobs.lock().unwrap();
            loop {
                if let Some(job) = jobs.pop_front() {
                    q.pending.fetch_sub(1, Ordering::Relaxed);
                    q.idle.fetch_sub(1, Ordering::Relaxed);
                    break 'get job;
                }
                jobs = q.ready.wait(jobs).unwrap();
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
    latch: Arc<Latch>,
}

struct Latch {
    /// Jobs still running, readable without the mutex so the waiter
    /// can spin: parking the caller on the condvar costs the same
    /// milliseconds of Windows wakeup the workers pay, and the jobs
    /// usually finish within microseconds of the caller's own work.
    left: AtomicUsize,
    mu: Mutex<()>,
    done: Condvar,
}

impl Latch {
    fn count_down(&self) {
        if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Taking the mutex orders this notify after any waiter's
            // check of `left`, so the wakeup cannot slip between the
            // check and the wait.
            drop(self.mu.lock().unwrap());
            self.done.notify_all();
        }
    }
}

impl Pending {
    pub(crate) fn wait(&self) {
        let start = std::time::Instant::now();
        while start.elapsed() < LATCH_SPIN {
            if self.latch.left.load(Ordering::Acquire) == 0 {
                return;
            }
            std::hint::spin_loop();
        }
        let mut guard = self.latch.mu.lock().unwrap();
        while self.latch.left.load(Ordering::Acquire) > 0 {
            guard = self.latch.done.wait(guard).unwrap();
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
///
/// The whole batch goes under one lock and one broadcast. Pushing them
/// one at a time and waking one worker each was a wakeup per job, and a
/// wakeup is not addressed to a job: a worker already awake takes the
/// job the notify was for, the notified worker finds the queue empty
/// and parks again, and the job it should have taken waits for whoever
/// finishes first. Timed per worker on a quiet 32 core box, that left
/// the last of eight starting after the other seven had finished the
/// scan between them, so a query asking for eight hands regularly got
/// seven, and the six before it started in a ramp a wakeup apart.
pub(crate) fn submit<'a>(jobs: Vec<Box<dyn FnOnce() + Send + 'a>>) -> Pending {
    let q = queue();
    let latch = Arc::new(Latch {
        left: AtomicUsize::new(jobs.len()),
        mu: Mutex::new(()),
        done: Condvar::new(),
    });
    if jobs.is_empty() {
        return Pending { latch };
    }
    let cap = std::thread::available_parallelism().map_or(1, |n| n.get());
    let count = jobs.len();
    // SAFETY: Pending waits, in wait() or in drop, until every job has
    // run, so the 'a borrows inside the jobs are live for as long as
    // any worker can touch them.
    let jobs: Vec<Job> = unsafe { std::mem::transmute(jobs) };
    let spawn = {
        let mut queued = q.jobs.lock().unwrap();
        for job in jobs {
            let latch = Arc::clone(&latch);
            let wrapped: Job = Box::new(move || {
                struct CountDown(Arc<Latch>);
                impl Drop for CountDown {
                    fn drop(&mut self) {
                        self.0.count_down();
                    }
                }
                let _count = CountDown(latch);
                job();
            });
            queued.push_back(wrapped);
        }
        let pending = q.pending.fetch_add(count, Ordering::Relaxed) + count;
        // Spawn while queued jobs outnumber the workers free to take
        // them; comparing against zero here left one parked worker
        // serving a whole batch alone, because idle only drops when a
        // worker actually dequeues, long after this has queued
        // everything.
        let free = q.idle.load(Ordering::Relaxed);
        let room = cap.saturating_sub(q.workers.load(Ordering::Relaxed));
        let spawn = pending.saturating_sub(free).min(room);
        q.workers.fetch_add(spawn, Ordering::Relaxed);
        spawn
    };
    // Outside the lock: a spawn takes long enough that holding the
    // queue through it would stall the workers already waiting to
    // dequeue, which is the opposite of what spawning is for.
    for _ in 0..spawn {
        std::thread::Builder::new()
            .name("zu-exec-worker".into())
            .spawn(move || worker_loop(q))
            .expect("spawn a pool worker");
    }
    q.ready.notify_all();
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

    /// Every job in a batch has to get a worker of its own, which is
    /// what a query asking for eight hands is asking for. Each job here
    /// counts itself in, waits for the rest to arrive, and counts itself
    /// out, so the high water mark is how many of them the pool had
    /// running at once. A batch that leaves one job in the queue while
    /// another worker runs two of them never reaches the mark, and the
    /// wait gives up on a timeout rather than hanging.
    #[test]
    fn a_batch_gets_a_worker_apiece() {
        let hands = std::thread::available_parallelism()
            .map_or(2, |n| n.get())
            .min(4);
        // Running now, the high water mark, and whether the mark was
        // ever reached, which the wait has to watch instead of the
        // count: the last job to arrive is also the first to leave.
        let state = (Mutex::new((0usize, 0usize, false)), Condvar::new());
        let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = (0..hands)
            .map(|_| {
                let state = &state;
                Box::new(move || {
                    let (mu, all_here) = state;
                    let mut count = mu.lock().unwrap();
                    count.0 += 1;
                    count.1 = count.1.max(count.0);
                    count.2 |= count.0 == hands;
                    all_here.notify_all();
                    while !count.2 {
                        let (next, gave_up) = all_here
                            .wait_timeout(count, std::time::Duration::from_secs(5))
                            .unwrap();
                        count = next;
                        if gave_up.timed_out() {
                            break;
                        }
                    }
                    count.0 -= 1;
                }) as Box<dyn FnOnce() + Send + '_>
            })
            .collect();
        submit(jobs).wait();
        assert_eq!(state.0.lock().unwrap().1, hands, "all {hands} ran at once");
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
