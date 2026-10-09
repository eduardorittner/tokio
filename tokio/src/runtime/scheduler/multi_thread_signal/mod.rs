//! Multi-threaded runtime that coordinates its workers with a signal tree.
//!
//! The flavor is a sibling of [`multi_thread`]. It keeps the local run queues,
//! the LIFO slot, the injection queue and the work stealing. It replaces the
//! idle-worker logic: a [`SignalSet`] records which work source holds work and
//! which worker sleeps, so a worker that looks for work never probes its peers.
//!
//! The data structure is a port of the `signal_tree` library of Michael
//! Maniscalco (<https://github.com/buildingcpp/work_contract>, MIT license).
//!
//! The flavor is **not** production ready. It is behind `tokio_unstable`.
//!
//! [`multi_thread`]: crate::runtime::scheduler::multi_thread
//! [`SignalSet`]: crate::util::signal_tree::SignalSet

mod handle;
pub(crate) use handle::Handle;

mod idle;
use self::idle::Idle;

#[cfg(all(test, loom))]
mod loom_signal_tree_idle;

mod worker;
pub(crate) use worker::{Context, Launch};

cfg_taskdump! {
    mod trace;
    use trace::TraceStatus;
}

cfg_not_taskdump! {
    mod trace_mock;
    use trace_mock::TraceStatus;
}

pub(crate) use worker::block_in_place;

// The queue, the parker, the per-worker statistics, the overflow trait and the
// internal counters do not depend on the flavor. Share them with
// `multi_thread` instead of keeping a second copy.
pub(crate) use super::multi_thread::{
    counters, park, queue, Counters, Overflow, Parker, Stats, Unparker,
};

use crate::loom::sync::Arc;
use crate::runtime::{
    blocking,
    driver::{self, Driver},
    scheduler, Config, TimerFlavor,
};
use crate::util::RngSeedGenerator;

use std::fmt;
use std::future::Future;

/// Work-stealing thread pool that uses a signal tree to find work.
pub(crate) struct MultiThreadSignal;

// ===== impl MultiThreadSignal =====

impl MultiThreadSignal {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        size: usize,
        driver: Driver,
        driver_handle: driver::Handle,
        blocking_spawner: blocking::Spawner,
        seed_generator: RngSeedGenerator,
        config: Config,
        timer_flavor: TimerFlavor,
        name: Option<String>,
    ) -> (MultiThreadSignal, Arc<Handle>, Launch) {
        let parker = Parker::new(driver);
        let (handle, launch) = worker::create(
            size,
            parker,
            driver_handle,
            blocking_spawner,
            seed_generator,
            config,
            timer_flavor,
            name,
        );

        (MultiThreadSignal, handle, launch)
    }

    /// Blocks the current thread waiting for the future to complete.
    ///
    /// The future will execute on the current thread, but all spawned tasks
    /// will be executed on the thread pool.
    pub(crate) fn block_on<F>(&self, handle: &scheduler::Handle, future: F) -> F::Output
    where
        F: Future,
    {
        crate::runtime::context::enter_runtime(handle, true, |blocking| {
            blocking.block_on(future).expect("failed to park thread")
        })
    }

    pub(crate) fn shutdown(&mut self, handle: &scheduler::Handle) {
        match handle {
            scheduler::Handle::MultiThreadSignal(handle) => handle.shutdown(),
            _ => panic!("expected MultiThreadSignal scheduler"),
        }
    }
}

impl fmt::Debug for MultiThreadSignal {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("MultiThreadSignal").finish()
    }
}
