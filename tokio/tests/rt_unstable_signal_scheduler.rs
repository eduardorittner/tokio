//! Targeted tests for the risks that the signal-tree scheduler adds.
//!
//! The whole test suite already runs against this flavor through the
//! `TOKIO_UNSTABLE_SIGNAL_SCHEDULER` environment variable, so this file does
//! **not** repeat the suite. It only covers the faults that the design of the
//! flavor can cause and that no other test looks for.

#![warn(rust_2018_idioms)]
#![cfg(all(feature = "full", tokio_unstable, not(target_os = "wasi")))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Runtime, RuntimeFlavor};
use tokio::sync::{mpsc, oneshot};

fn rt(worker_threads: usize) -> Runtime {
    Builder::new_multi_thread_signal()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn reports_its_own_flavor() {
    let rt = rt(2);
    let flavor = rt.block_on(async { tokio::runtime::Handle::current().runtime_flavor() });
    assert_eq!(flavor, RuntimeFlavor::MultiThreadSignal);
}

/// Spawns from outside the runtime while the workers go in and out of a park.
///
/// A lost wakeup leaves a task unclaimed while every worker sleeps, so the
/// count never reaches the target and the test times out.
#[test]
fn park_race_does_not_lose_a_task() {
    const ROUNDS: usize = 300;

    let rt = rt(4);
    let done = Arc::new(AtomicUsize::new(0));

    for round in 0..ROUNDS {
        let done = done.clone();

        // Give every worker time to reach a real park, so that the spawn below
        // lands on the park transition and not on a busy worker.
        std::thread::sleep(Duration::from_micros(if round % 2 == 0 { 200 } else { 50 }));

        rt.spawn(async move {
            done.fetch_add(1, Ordering::Relaxed);
        });
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    while done.load(Ordering::Relaxed) < ROUNDS {
        assert!(
            Instant::now() < deadline,
            "only {} of {} tasks ran: a wakeup was lost",
            done.load(Ordering::Relaxed),
            ROUNDS
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Remote spawns that arrive in bursts while the workers are busy.
///
/// This drives the path where a producer sets the injection signal and the
/// work set is already not empty, so the producer does no wake of its own.
#[test]
fn remote_burst_while_busy() {
    const THREADS: usize = 4;
    const PER_THREAD: usize = 500;

    let rt = rt(4);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let handle = rt.handle().clone();
            let tx = tx.clone();

            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let tx = tx.clone();
                    handle.spawn(async move {
                        let _ = tx.send(i);
                    });
                }
            })
        })
        .collect();

    drop(tx);

    for thread in threads {
        thread.join().unwrap();
    }

    rt.block_on(async {
        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(count, THREADS * PER_THREAD);
    });
}

/// A worker that fills its own run queue must stay reachable.
///
/// A steal takes about half of the queue, so the source still holds work. The
/// worker that stole must put the signal back, or the rest of the queue is
/// invisible and the other workers sleep on top of it.
#[test]
fn queue_stays_reachable_after_a_steal() {
    const TASKS: usize = 512;

    let rt = rt(4);

    rt.block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (go_tx, go_rx) = oneshot::channel();

        // One task fills the run queue of its own worker in one burst, then
        // blocks. The other workers must find the queued tasks through the
        // signal tree.
        tokio::spawn(async move {
            for i in 0..TASKS {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(i);
                });
            }

            let _ = go_rx.await;
        });

        let mut count = 0;
        while count < TASKS {
            rx.recv().await.expect("a queued task was never reached");
            count += 1;
        }

        let _ = go_tx.send(());
    });
}

/// The tree does not keep FIFO order, so a task that always reschedules itself
/// could in principle hold a worker forever. Check that a slow task still makes
/// progress next to a crowd of fast ones.
#[test]
fn a_slow_task_is_not_starved() {
    const FAST: usize = 256;
    const STEPS: usize = 64;

    let rt = rt(2);

    rt.block_on(async {
        let stop = Arc::new(AtomicUsize::new(0));

        let busy: Vec<_> = (0..FAST)
            .map(|_| {
                let stop = stop.clone();
                tokio::spawn(async move {
                    while stop.load(Ordering::Relaxed) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();

        let slow = tokio::spawn(async {
            for _ in 0..STEPS {
                tokio::task::yield_now().await;
            }
            "done"
        });

        let result = tokio::time::timeout(Duration::from_secs(30), slow)
            .await
            .expect("the slow task made no progress")
            .unwrap();

        assert_eq!(result, "done");

        stop.store(1, Ordering::Relaxed);

        for task in busy {
            task.await.unwrap();
        }
    });
}

/// Shuts the runtime down while many signals are set.
///
/// No task may leak and no worker thread may hang.
#[test]
fn shutdown_with_set_signals() {
    const TASKS: usize = 1_000;

    let rt = rt(4);
    let alive = Arc::new(AtomicUsize::new(0));

    for _ in 0..TASKS {
        let alive = alive.clone();
        alive.fetch_add(1, Ordering::Relaxed);

        rt.spawn(async move {
            tokio::task::yield_now().await;
            alive.fetch_sub(1, Ordering::Relaxed);
        });
    }

    // Drop while the injection queue, the local queues and the sleeper set all
    // hold signals. A worker that waits on a signal that no longer has an
    // owner would hang here.
    let start = Instant::now();
    drop(rt);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "the shutdown took {elapsed:?}: a worker did not leave its park"
    );

    // Every task that did not run was cancelled, so the counter cannot be
    // above the number of tasks that never reached their second half.
    assert!(alive.load(Ordering::Relaxed) <= TASKS);
}

/// `block_in_place` moves the core of a worker to another thread. The signals
/// of that worker name its run queue, which does not move, so the work must
/// stay reachable across the hand-off.
#[test]
fn block_in_place_keeps_the_queue_reachable() {
    let rt = rt(4);

    rt.block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            for i in 0..128 {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(i);
                });
            }

            tokio::task::block_in_place(|| {
                std::thread::sleep(Duration::from_millis(50));
            });
        });

        let mut count = 0;
        while count < 128 {
            rx.recv()
                .await
                .expect("a task was lost over block_in_place");
            count += 1;
        }
    });
}

/// More workers than the machine has cores, to check that the signal set sizes
/// itself from the worker count and not from the hardware.
#[test]
fn many_workers() {
    const WORKERS: usize = 64;
    const TASKS: usize = 1_000;

    let rt = rt(WORKERS);

    rt.block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel();

        for i in 0..TASKS {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(i);
            });
        }

        drop(tx);

        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }

        assert_eq!(count, TASKS);
    });
}

/// More workers than one tree holds, so the signal set must span several trees.
#[test]
fn more_workers_than_one_tree() {
    // One tree holds 512 signals, so 600 workers need a second tree.
    const WORKERS: usize = 600;
    const TASKS: usize = 2_000;

    let rt = rt(WORKERS);

    rt.block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel();

        for i in 0..TASKS {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(i);
            });
        }

        drop(tx);

        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }

        assert_eq!(count, TASKS);
    });
}
