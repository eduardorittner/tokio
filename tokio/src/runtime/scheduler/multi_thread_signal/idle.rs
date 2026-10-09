//! Coordinates idling workers with a pair of signal trees.
//!
//! The flavor keeps two sets of signals:
//!
//! * The **work** set holds one signal for each work source: the run queue of
//!   each worker, the injection queue and the remote timer list. A producer
//!   sets the signal of the source that it filled. A worker that looks for work
//!   claims one signal and steals from that one source. It never probes its
//!   peers.
//! * The **sleepers** set holds one signal for each worker that sleeps. A
//!   producer that must wake a worker claims one signal and unparks that
//!   worker.
//!
//! # Why there is no lost wakeup
//!
//! The park path and the notify path form a Dekker pair:
//!
//! | Worker that parks | Producer |
//! |---|---|
//! | set its sleeper signal | set the work signal |
//! | `SeqCst` fence | `SeqCst` fence |
//! | read the work root | read the sleepers root |
//!
//! Each side writes its own root before it reads the root of the other side,
//! and a `SeqCst` fence separates the write from the read. At most one of the
//! two reads can miss the write of the other side, so at least one side acts:
//! either the worker sees the work and does not sleep, or the producer sees the
//! sleeper and wakes it.
//!
//! The park state of a worker is a second, smaller guard. A sleeper signal can
//! be stale: a worker that wakes from the driver resumes while its signal is
//! still set. A producer that claims a stale signal fails the
//! `PARKED -> NOTIFIED` compare-and-swap and claims the next signal, so a stale
//! signal costs one claim and then goes away.

use crate::loom::sync::atomic::{fence, AtomicUsize};
use crate::util::cacheline::CachePadded;
use crate::util::signal_tree::SignalSet;

use std::sync::atomic::Ordering::{AcqRel, Acquire, SeqCst};

/// The worker runs.
const AWAKE: usize = 0;

/// The worker published that it sleeps. It is claimable.
const PARKED: usize = 1;

/// A producer claimed the worker and unparked it. The producer already counted
/// the worker as a searcher.
const NOTIFIED: usize = 2;

pub(super) struct Idle {
    /// One signal for each work source.
    work: SignalSet,

    /// One signal for each worker that sleeps.
    sleepers: SignalSet,

    /// The park state of each worker.
    states: Box<[CachePadded<AtomicUsize>]>,

    /// The number of workers that look for work.
    num_searching: AtomicUsize,

    /// The number of workers that sleep.
    num_parked: AtomicUsize,

    /// Total number of workers.
    num_workers: usize,
}

impl Idle {
    pub(super) fn new(num_workers: usize) -> Idle {
        let mut states = Vec::with_capacity(num_workers);
        for _ in 0..num_workers {
            states.push(CachePadded::new(AtomicUsize::new(AWAKE)));
        }

        Idle {
            // One signal for each worker, one for the injection queue and one
            // for the remote timer list.
            work: SignalSet::with_capacity(num_workers + 2),
            sleepers: SignalSet::with_capacity(num_workers),
            states: states.into_boxed_slice(),
            num_searching: AtomicUsize::new(0),
            num_parked: AtomicUsize::new(0),
            num_workers,
        }
    }

    // ===== the work set =====

    /// The signal of the injection queue.
    pub(super) fn inject_signal(&self) -> usize {
        self.num_workers
    }

    /// The signal of the remote timer list.
    #[cfg_attr(not(all(tokio_unstable, feature = "time")), allow(dead_code))]
    pub(super) fn timer_signal(&self) -> usize {
        self.num_workers + 1
    }

    /// Reports that the source `id` holds work.
    ///
    /// Returns `true` when the call changed the work set from empty to not
    /// empty.
    pub(super) fn set_work(&self, id: usize) -> bool {
        self.work.set(id)
    }

    /// Claims the signal of one source that holds work.
    ///
    /// The caller must service that source. When the source still holds work
    /// after the service, the caller sets the signal again.
    pub(super) fn select_work(&self, hint: &mut usize) -> Option<usize> {
        self.work.select(hint)
    }

    /// Returns `true` when a source holds work.
    pub(super) fn has_work(&self) -> bool {
        !self.work.is_empty()
    }

    // ===== the sleepers set =====

    /// Returns the index of a worker that sleeps, and marks that worker as
    /// notified.
    ///
    /// The caller must unpark the returned worker.
    pub(super) fn worker_to_notify(&self) -> Option<usize> {
        // Make the work signal of the caller visible to a worker that is on
        // its way into a park. See the Dekker pair in the module comment.
        fence(SeqCst);

        // A worker counts itself here **before** it puts its signal into the
        // set, so a count of zero means that the set holds no signal of a
        // worker that sleeps. The read is part of the same Dekker pair as the
        // read of the root below, so a zero answer is safe: the worker that is
        // on its way into a park then sees the work of this caller.
        if self.num_parked.load(SeqCst) == 0 {
            return None;
        }

        let mut hint = 0;

        // A claimed signal can be stale. Each turn of this loop takes one
        // signal out of the set, so the loop ends.
        while let Some(index) = self.sleepers.select(&mut hint) {
            if self.states[index]
                .compare_exchange(PARKED, NOTIFIED, AcqRel, Acquire)
                .is_ok()
            {
                // The worker counts as a searcher from now on. It reads this
                // as `is_searching` when it wakes.
                self.num_searching.fetch_add(1, SeqCst);
                self.num_parked.fetch_sub(1, SeqCst);
                return Some(index);
            }

            // The signal was stale: the worker is already awake.
        }

        None
    }

    /// Publishes that the worker sleeps.
    ///
    /// The caller must check [`Idle::has_work`] after this call and wake a
    /// worker when the answer is `true`.
    pub(super) fn transition_worker_to_parked(&self, worker: usize) {
        // The state must become claimable **before** the signal goes into the
        // set. A producer reads the signal first and the state second, so the
        // other order would let a producer see the signal, read `AWAKE`, and
        // drop the wake.
        let prev = self.states[worker].swap(PARKED, SeqCst);
        debug_assert_eq!(prev, AWAKE, "the worker was already published as a sleeper");

        // The count goes up before the signal, so a producer that reads a
        // count of zero is in the same Dekker pair as the read of the root.
        self.num_parked.fetch_add(1, SeqCst);

        // A set is idempotent, so a signal that is still in the set from an
        // earlier park is not set a second time. That is safe: the only way a
        // signal leaves the set is a claim, and a claim that finds this worker
        // awake is ordered before this publication, so the claim that follows
        // it reads the state that this call just wrote.
        self.sleepers.set(worker);

        // Order this publication before the `has_work` read of the caller.
        fence(SeqCst);
    }

    /// Returns `true` when a producer notified the worker.
    pub(super) fn is_notified(&self, worker: usize) -> bool {
        self.states[worker].load(Acquire) == NOTIFIED
    }

    /// Takes the worker out of the sleeping state.
    ///
    /// Returns `true` when a producer had notified the worker. The caller then
    /// enters the searching state, because the producer already counted it.
    ///
    /// The call is idempotent.
    pub(super) fn transition_worker_from_parked(&self, worker: usize) -> bool {
        let prev = self.states[worker].swap(AWAKE, SeqCst);

        if prev == PARKED {
            self.num_parked.fetch_sub(1, SeqCst);
        }

        prev == NOTIFIED
    }

    // ===== searching =====

    /// Enters the searching state.
    ///
    /// The old scheduler stops a worker here when half of the workers already
    /// search, because each searcher probes every peer. A searcher of this
    /// flavor reads the work set instead, so it does no failed probe and the
    /// limit has no purpose.
    pub(super) fn transition_worker_to_searching(&self) -> bool {
        self.num_searching.fetch_add(1, SeqCst);
        true
    }

    /// Leaves the searching state.
    ///
    /// Returns `true` when this was the last searching worker. The caller
    /// **must** then wake another worker.
    pub(super) fn transition_worker_from_searching(&self) -> bool {
        self.num_searching.fetch_sub(1, SeqCst) == 1
    }
}
