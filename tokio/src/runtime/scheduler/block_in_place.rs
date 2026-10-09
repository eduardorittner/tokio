use crate::runtime::scheduler;

#[track_caller]
pub(crate) fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    // The two multi-thread flavors keep their worker core in a different
    // thread-local slot, so the call goes to the flavor of the current
    // runtime. Outside a runtime both calls do the same thing, so the default
    // flavor is a safe answer.
    #[cfg(tokio_unstable)]
    {
        let is_signal = crate::runtime::context::with_scheduler(|ctx| {
            matches!(ctx, Some(scheduler::Context::MultiThreadSignal(_)))
        });

        if is_signal {
            return scheduler::multi_thread_signal::block_in_place(f);
        }
    }

    scheduler::multi_thread::block_in_place(f)
}
