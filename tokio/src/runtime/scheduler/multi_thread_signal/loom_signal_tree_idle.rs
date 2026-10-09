//! `loom` tests of the park and notify protocol of [`Idle`].
//!
//! The structure tests in `util::signal_tree::loom_signal_tree` cover the tree
//! itself. These tests cover the pair of trees and the per-worker park state as
//! the scheduler uses them.

use super::idle::Idle;

use loom::sync::atomic::AtomicUsize;
use loom::sync::Arc;
use loom::thread;

use std::sync::atomic::Ordering::SeqCst;

/// A worker that is on its way into a park must never sleep while a producer
/// holds work that no other worker can see.
///
/// A failure of this test is a runtime that hangs.
#[test]
fn no_lost_wakeup() {
    loom::model(|| {
        let idle = Arc::new(Idle::new(1));

        // `1` when the worker reached the point where it blocks.
        let slept = Arc::new(AtomicUsize::new(0));
        // `1` when a producer claimed the worker.
        let notified = Arc::new(AtomicUsize::new(0));

        let producer = {
            let idle = idle.clone();
            let notified = notified.clone();

            thread::spawn(move || {
                // Stands for a push into the injection queue.
                idle.set_work(idle.inject_signal());

                if idle.worker_to_notify().is_some() {
                    notified.store(1, SeqCst);
                }
            })
        };

        let worker = {
            let idle = idle.clone();
            let slept = slept.clone();

            thread::spawn(move || {
                idle.transition_worker_to_parked(0);

                if idle.has_work() {
                    // The worker found the work and does not sleep.
                    return;
                }

                slept.store(1, SeqCst);
            })
        };

        producer.join().unwrap();
        worker.join().unwrap();

        assert!(
            notified.load(SeqCst) == 1 || slept.load(SeqCst) == 0,
            "the worker slept and the producer did not wake it"
        );
    });
}

/// A producer must not drop a wake because the sleeper signal of the worker is
/// stale. The worker leaves the parked state without taking its signal out of
/// the set, so the producer reads the park state as well.
#[test]
fn stale_sleeper_signal() {
    loom::model(|| {
        let idle = Arc::new(Idle::new(1));

        // Leave a stale signal: the worker parks and then leaves on its own,
        // as it does after a wake from the I/O driver.
        idle.transition_worker_to_parked(0);
        assert!(!idle.transition_worker_from_parked(0));

        let producer = {
            let idle = idle.clone();
            thread::spawn(move || idle.worker_to_notify().is_some())
        };

        let claimed = producer.join().unwrap();

        // The worker is awake, so there is nobody to claim.
        assert!(!claimed, "a producer claimed a worker that is awake");
    });
}

/// Two producers must not hand the same sleeping worker out twice.
#[test]
fn two_producers_one_sleeper() {
    loom::model(|| {
        let idle = Arc::new(Idle::new(1));

        idle.transition_worker_to_parked(0);

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let idle = idle.clone();
                thread::spawn(move || usize::from(idle.worker_to_notify().is_some()))
            })
            .collect();

        let claims: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();

        assert_eq!(claims, 1, "the sleeping worker was handed out twice");
    });
}
