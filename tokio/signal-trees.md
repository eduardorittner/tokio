<!--
  DELETE THIS FILE BEFORE YOU OPEN A PULL REQUEST AGAINST tokio-rs/tokio.
-->

> **Delete this file before you open a pull request.**
>
> This is a working document for the `multi_thread_signal` branch. It is not
> part of Tokio. It stays in the tree only to keep the design, the test plan
> and the benchmark plan next to the code while the work goes on.
>
> To remove it:
>
> ```bash
> git rm tokio/signal-trees.md
> ```

---

# Signal Trees as an Alternative Tokio Scheduler

Status: Phase 1, Phase 2 and Phase 3 are done. Date: 2026-10-09.
Base commit: `df6e04f2`.

This document is written in ASD-STE100 Simplified Technical English.

---

## 0. Implementation Status

### Done

| Phase | Result |
|---|---|
| 1. The signal tree module | `tokio/src/util/signal_tree/`. 15 unit tests and 5 loom tests pass. |
| 2. The flavor skeleton | `scheduler/multi_thread_signal/`. All change points of Section 4.2 are made. `TOKIO_UNSTABLE_SIGNAL_SCHEDULER` runs the whole suite. |
| 3. Answer A | `Idle` is now two signal sets. The probe, the sleeper mutex and the full scan are gone. |

Test results:

| Test set | Result |
|---|---|
| The whole suite with `TOKIO_UNSTABLE_SIGNAL_SCHEDULER=1` | 1653 pass |
| The whole suite with the default flavor | 1653 pass |
| The whole suite with no `tokio_unstable` | 1526 pass |
| The doc tests, both flavors | 791 pass |
| `loom`, the structure and `Idle`, `LOOM_MAX_PREEMPTIONS=3` | 8 pass |
| `loom`, the scheduler, `LOOM_MAX_PREEMPTIONS=1` | 14 pass |
| The targeted tests of Section 7.7 | 9 pass |
| `cargo hack`, 91 feature sets, `-Dwarnings` | clean |
| The 32-bit target with no `AtomicU64` | clean |
| `taskdump` on `x86_64-unknown-linux-gnu` | clean |

### Deviations from the plan

1. **Shared modules.** `queue.rs`, `park.rs`, `stats.rs`, `overflow.rs` and
   `counters.rs` come from `multi_thread`. Only `worker.rs`, `handle.rs`,
   `idle.rs` and the trace files are new. This follows the last row of the
   risk table of Section 9.
2. **The blocking state.** The plan puts a futex or a condition variable in
   `SignalSet` (Section 2.8). The flavor uses the `Parker` of Tokio instead,
   and a second signal set gives the id of the worker to wake. A second
   blocking primitive would duplicate the parker.
3. **The benchmark names.** Section 8.2 asks for one Criterion group for both
   flavors. The default flavor keeps its old benchmark name and the new flavor
   adds the suffix `_signal`. A Criterion group changes the name of the default
   flavor, which breaks the Phase 0 baseline that Section 8.5 needs.
4. **`benches/signal_tree.rs` is not written.** The structure is
   `pub(crate)`. A benchmark crate cannot reach it. To add the microbenchmarks
   of Section 8.1 you must first make a `#[doc(hidden)]` public path to the
   module. This is a change to the public surface of Tokio, so it is a decision
   for the maintainers.

### Not done

* **Phase 0 and Phase 4.** No baseline and no gate. Section 8.5 needs a pinned
  machine with the turbo mode off. Do these on such a machine.
* **Phase 5.** Answer B. Section 5.2 gates it on Phase 4.
* **Phase 6.** The selector option, the signal-tree metrics and the counters of
  Section 8.6.

---

## 1. Purpose

This plan tells you how to add a new multi-thread scheduler to Tokio. The new
scheduler uses a **signal tree** in place of the current idle-worker logic.

The plan has five parts:

1. The research results for signal trees.
2. The research results for the current Tokio scheduler.
3. The research results for the removed alternative scheduler.
4. The design and the work phases.
5. The test plan and the benchmark plan.

---

## 2. Research: Signal Trees

### 2.1 Source of the concept

Michael Maniscalco made the signal tree. He showed it at C++Now 2026 in the
talk "Signal Trees". He is a principal engineer in low-latency finance systems.

The reference code is in C++20 at `github.com/buildingcpp/work_contract`. The
code is under the MIT license. Tokio is also under the MIT license, so the two
licenses agree. Give credit to the author in the new module.

The repository holds two libraries:

- **Signal Tree**: a concurrent readiness set. It selects a set signal fast.
  It does not keep FIFO order.
- **Work Contract**: a store of persistent tasks. It schedules the tasks
  through a signal tree. The application owns the worker threads.

The author claims up to 40 times more throughput than a queue under contention.
Treat this claim as unproven for Tokio until you measure it.

### 2.2 The problem that a signal tree solves

A scheduler must answer one question: *which ready source do I service next?*
Most schedulers answer this question with a queue. A queue has two weak points:

- A fan-in queue puts all contention on one cache line. Throughput falls when
  the thread count increases.
- Priority adds more complexity and more contention.

A signal tree answers the same question with a tree of atomic words. No thread
polls. No thread waits for a lock.

### 2.3 The data structure

A signal tree is an array of 64-bit atomic words. Each word is on its own cache
line. The tree has a fixed depth and a fixed capacity.

**Level 0 (the leaves).** Each leaf word is a bitset of 64 signals. Bit *i* is
1 when signal *i* is ready.

**Level 1 and above (the branches).** Each branch word holds a group of
counters. The code calls each counter a *lane*. Each lane counts the set
signals in one child subtree. The lane width is the next power of two above the
bit width of the child capacity.

This table gives the layout:

| Depth | Lane width | Lanes per word | Capacity | Words |
|---|---|---|---|---|
| 0 | 1 bit | 64 | 64 | 1 |
| 1 | 8 bits | 8 | 512 | 9 |
| 2 | 16 bits | 4 | 2048 | 37 |
| 3 | 16 bits | 4 | 8192 | 149 |

A depth-1 tree needs 9 cache lines for 512 signals. The memory cost is about
1.1 bytes for each signal.

### 2.4 The set operation

A producer calls `set(id)` to make a signal ready:

1. At the leaf, the producer does `fetch_or(bit, Release)`.
2. If the bit was already 1, the operation stops. The set is **idempotent**.
3. If the bit was 0, the producer walks up the tree. At each branch it does
   `fetch_add(1 << (lane * lane_width), Release)`.
4. At the root, the return value shows if the tree changed from empty to
   non-empty.

Point 4 is important. Only the thread that sees the empty-to-non-empty change
must wake a sleeping worker. All other producers do no system call.

### 2.5 The select operation

A worker calls `select(hint)` to claim a ready signal:

1. The worker starts at the root. It loads the branch word.
2. A *selector* policy picks one non-empty lane.
3. The worker does a compare-and-swap that decrements that lane. This
   **reserves** one signal in that subtree. Two workers cannot reserve the same
   signal. On failure the worker reloads the word and tries again.
4. The worker repeats steps 2 and 3 at each level, down to the leaf.
5. At the leaf, the worker finds a set bit with a count-trailing-zeros
   instruction. It claims the bit with `fetch_and(!bit, AcqRel)`.
6. The call returns the signal id. It also returns a flag that shows if the
   root became empty.

The reservation at step 3 is the key idea. A worker never descends into a
subtree that holds no work. The cost is O(depth), not O(number of sources).

### 2.6 The hint

Each worker keeps a `hint`. The hint is a cursor into the id space. The worker
passes the hint to `select` and `select` updates it. The hint gives two
benefits:

- **Locality**: the worker continues near its last position.
- **Fairness**: the worker does not always start at signal 0.

### 2.7 The selector policies

The reference code has two policies:

- `fairness_selector`: a tournament that uses the hint as a bias. It also
  keeps a mask for the next hint.
- `densest_child_selector`: it adds the lane counters of each half and goes to
  the fuller half.

A new policy is a small struct. This is how priority enters the design. The
policy, not the structure, decides the order.

### 2.8 The blocking state

The reference code wraps one or more trees in a `signal_set`. The set holds a
blocking state with three fields:

- a count of non-empty trees,
- a wake sequence number,
- a stop flag.

`set` increments the count when a tree changes from empty to non-empty. Only
the increment from 0 wakes the sleepers. `select` decrements the count when a
tree becomes empty. The Linux version uses a futex. The portable version uses a
mutex and a condition variable.

### 2.9 The re-signal pattern

In `work_contract`, each task slot holds flags. The flags are `schedule` and
`execute`. A worker clears `execute` after it runs the task. If `schedule` is
still set, the worker sets the signal again. This pattern lets a task reschedule
itself while it runs.

Tokio already has the same idea in the task state. The `NOTIFIED` bit does the
same job.

### 2.10 Properties to remember

| Property | Value |
|---|---|
| Order | Not FIFO |
| Set | Idempotent |
| Capacity | Fixed at construction |
| Claim cost | O(depth), 1 CAS for each level |
| Wake cost | 1 system call for each empty-to-non-empty change |
| Growth | Add more trees and scan them in turn |

The fixed capacity is the main point of friction with Tokio. Section 4.2
explains it.

---

## 3. Research: The Current Tokio Multi-Thread Scheduler

Source: `tokio/src/runtime/scheduler/multi_thread/`, 3464 lines.

### 3.1 The parts

| File | Lines | Job |
|---|---|---|
| `worker.rs` | 1566 | The worker loop, `Core`, `Shared`, `Handle` |
| `queue.rs` | 602 | The local run queue, 256 slots, fixed size |
| `park.rs` | 316 | Park and unpark of a worker thread |
| `idle.rs` | 248 | The coordination of idle workers |
| `stats.rs` | 149 | The statistics for each worker |
| `handle.rs` | 141 | The scheduler handle |
| `mod.rs` | 106 | The flavor entry point |
| `overflow.rs` | 26 | The overflow of a full local queue |

The injection queue is in `tokio/src/runtime/scheduler/inject/`, 503 lines.

### 3.2 `Core`: the state of one worker

`Core` holds:

- `lifo_slot`: one task slot. The worker runs this task first.
- `run_queue`: a work-stealing queue with 256 slots.
- `is_searching`: true when the worker looks for work to steal.
- `park`: the parker.
- `stats`, `tick`, `global_queue_interval`, `rand`.

### 3.3 `Shared`: the state of all workers

`Shared` holds:

- `remotes`: a steal handle and an unparker for each worker.
- `inject`: the global injection queue.
- `idle`: the coordination of idle workers.
- `owned`: all live tasks, in a sharded list.
- `synced`: a mutex that protects the sleeper list.

### 3.4 How a worker finds work

`Core::next_task` runs this order:

1. Every `global_queue_interval` ticks, read the injection queue first.
2. Else read the LIFO slot, then the local queue.
3. Else take a batch from the injection queue.

`Core::steal_work` runs when all of the above give nothing:

1. Ask `Idle` for permission to search. At most half of the workers may search.
2. Pick a random start index.
3. Probe **every** peer in turn until one gives a task.
4. Fall back to the injection queue.

### 3.5 How a producer wakes a worker

`Handle::schedule_task` sends a task to the local queue if the caller is a
worker of the same runtime. Else it pushes to the injection queue and calls
`notify_parked_remote`.

`Idle::worker_to_notify` does this:

1. A `SeqCst` read-modify-write on `self.state`. The code uses
   `fetch_add(0, SeqCst)`, not a load. The comment says that a load gives weak
   guarantees. This operation makes the push visible to a worker that parks.
2. If a worker searches now, return `None`. No wake is needed.
3. Else take the scheduler mutex.
4. Check the state again.
5. Pop one worker id from `sleepers` and unpark that worker.

### 3.6 The weak points

These are the costs that a signal tree can remove:

| Weak point | Where | Cost |
|---|---|---|
| A `SeqCst` RMW for each remote schedule | `idle.rs:161` | One full barrier on a shared line |
| A mutex for each park and each unpark | `idle.rs:66,93` | Lock contention at high core counts |
| A random probe of all peers | `worker.rs:1167` | O(workers) failed steals |
| A scan of all peers at the last park | `worker.rs:1483` | O(workers) loads |
| A limit of 50% searchers | `idle.rs:110` | A fix for the probe cost, not for the cause |

The signal tree removes the cause of the last three. The tree tells a worker
**which** source holds work. The worker does not probe.

---

## 4. Research: The Removed Alternative Scheduler

### 4.1 What happened

Carl Lerche added the alternative scheduler in `4165601b` (PR #5823) in 2023.
He removed it in `159a3b2c` (PR #7275) in April 2025. The commit message says:

> The alternative multi-threaded runtime started as an experiment. We have been
> unable to find real-world benefit. Work has halted on this effort, so lets get
> rid of it.

The removal deleted 5444 lines across 33 files. Two later commits cleaned up the
tests: `7dd4d8a3` (PR #7451) and `154d7d5f` (PR #7457).

The builder documentation said that the scheduler "currently does not scale as
well to 16+ processors". Treat this as a warning. Any new flavor must prove
itself at 16, 32 and 64 cores.

### 4.2 How it was wired

This is the exact blueprint for the new flavor. Each row is a change point.

| File | Change |
|---|---|
| `scheduler/mod.rs` | A `MultiThreadAlt` arm in `Handle` and in `Context`. An arm in `match_flavor!`. Arms in `driver`, `is_local`, `spawn`, `shutdown`, `hooks`, `num_workers`. An `expect_multi_thread_alt` helper. |
| `builder.rs` | A `MultiThreadAlt` value in `Kind`. A `new_multi_thread_alt()` constructor under `cfg_unstable!`. An arm in `build()`. A `build_alt_threaded_runtime()` method. |
| `runtime.rs` | A `MultiThreadAlt` value in `RuntimeFlavor` and in `Scheduler`. Arms in `block_on` and in `Drop`. |
| `handle.rs` | Arms in `runtime_flavor()`, `owned_id()` and the task dump. |
| `blocking/schedule.rs` | Two empty match arms. |
| `scheduler/block_in_place.rs` | A dispatch on the flavor. |
| `config.rs` | A `local_queue_capacity` field. |
| `driver.rs` | An `is_enabled()` method. |
| `inject/synced.rs` | An `is_empty()` method. |
| `task/trace/mod.rs` | Two lines. |

The scheduler itself was a new directory, `scheduler/multi_thread_alt/`, with
24 files and about 4500 lines.

### 4.3 How it was tested

| Place | Method |
|---|---|
| `tokio/tests/rt_threaded_alt.rs` | A 744-line copy of `rt_threaded.rs` |
| `tokio/tests/rt_common.rs` | Two more modules in the `rt_test!` macro |
| `tokio/src/runtime/tests/loom_multi_thread_alt/` | A copy of the loom tests, 842 lines |
| `.github/workflows/loom.yml` | A job with four groups, a to d |
| The label `R-loom-multi-thread-alt` | It started the loom job on a pull request |

### 4.4 Lessons

1. The flavor sat behind `tokio_unstable` and a feature gate. Do the same.
2. The flavor reused the test suite through the `rt_test!` macro and through a
   744-line copy of `rt_threaded.rs`. The copy was waste. Tokio now has a better
   method: an environment variable that changes the default flavor. The method
   needs no test change at all. Section 7.6 gives the detail.
3. The project removed the flavor because it gave no benefit. **Set the
   benchmark targets before you write code.** Section 8 does this.
4. The flavor was a full copy of the scheduler. The copy made maintenance
   expensive. Share more code this time (Section 6.3).

---

## 5. Design

### 5.1 The name

Call the flavor `multi_thread_signal`. Call the builder method
`Builder::new_multi_thread_signal()`. Call the enum value
`RuntimeFlavor::MultiThreadSignal`. Put all of it behind `tokio_unstable` and
behind the `rt-multi-thread` feature.

### 5.2 The central question: what is a signal?

A signal tree needs a dense, fixed id space. A Tokio runtime holds a dynamic and
unbounded set of tasks. There are two answers to this conflict.

**Answer A: one signal for each work source.**

The id space is the set of work sources: the *N* worker queues, the injection
queue, and the timer source. *N* is small and fixed. A depth-0 tree holds 64
sources. A depth-1 tree holds 512 sources.

- The tree replaces `Idle` and the random probe.
- The local queues, the LIFO slot and the task state do not change.
- The risk is low. The change is small.

**Answer B: one signal for each task.**

The id space is a slot table of tasks. The tree replaces the local queues, the
injection queue and the work stealing.

- This matches the reference design most closely.
- It needs a slot allocator with stable addresses and generation counters.
- It needs safe memory reclamation for the slot table.
- `OwnedTasks` is a sharded linked list today. It gives no dense index. You must
  add one.
- The risk is high. The change is large.

**The decision: do Answer A first. Gate Answer B on the measurements.**

Answer A attacks the five weak points in Section 3.6 with about 400 lines of new
code. Answer B is a rewrite. If Answer A shows no gain, Answer B is very
unlikely to show one, and you learn this at a much lower cost.

### 5.3 Answer A in detail

Give each work source one signal id:

| Id range | Source |
|---|---|
| `0 .. N` | The run queue of worker *i* |
| `N` | The injection queue |
| `N + 1` | The remote timer list, when the alternative timer is on |

**The producer side.** A producer sets the signal of the source that it filled:

- A worker that pushes to its own empty run queue sets signal *i*.
- A thread that pushes to the injection queue sets signal *N*.
- `set` returns the empty-to-non-empty flag. Only on that flag does the producer
  wake one parked worker.

**The consumer side.** A worker with no local work calls `select`:

- The tree returns the id of a source that holds work.
- The worker steals from that source only. It does no probe.
- If the source still holds work after the steal, the worker sets the signal
  again. This is the re-signal pattern from Section 2.9.
- If `select` returns the empty flag for the root, the worker parks.

**What this removes:**

- The `sleepers` vector and its mutex.
- The `SeqCst` read-modify-write for each remote schedule.
- The random probe loop in `steal_work`.
- The full scan in `notify_if_work_pending`.
- The 50% limit on searchers. The tree makes the limit unnecessary, because a
  searcher never does a failed probe.

**What this keeps:**

- The task state machine and the `NOTIFIED` bit.
- The LIFO slot and its locality benefit.
- The local queues and the overflow path.
- The coop budget and `global_queue_interval`.
- The metrics, the task dump and `block_in_place`.

### 5.4 Answer B in detail (the gated phase)

If Answer A succeeds, add a second mode:

- Add a slot table. Use a chunked array, so that growth does not move a live
  slot. Each chunk is a fixed array of `AtomicPtr<Header>` and a generation
  count.
- Use a second signal tree as a free-slot allocator. The reference code does
  this with its `available_` set.
- `Schedule::schedule` sets the signal of the task. It does not push to a queue.
- A worker selects a signal, reads the slot, and polls the task.
- The generation count prevents an ABA fault when a slot is reused.

Note: a per-task tree and the Tokio task state hold the same fact twice. The
`NOTIFIED` bit and the leaf bit both say "this task is ready". Make one of them
authoritative. The leaf bit should be the authority inside this flavor.

### 5.5 Rust specifics

**Atomic width.** The structure uses `AtomicU64` always. The node width is 64
bits on every target. Do **not** make the word type generic over `usize`.

Tokio has an `AtomicU64` shim in `tokio/src/loom/std/atomic_u64.rs`. On a target
with no 64-bit atomic, the shim uses a mutex. This is acceptable. A 32-bit
target is rare. Such a target has a special use, and this scheduler aims at the
general use, which is a 64-bit target.

This decision gives three benefits:

- The layout stays the same on all targets. The table of Section 2.3 is the
  only layout. There is one code path to read, to test and to measure.
- The lane widths stay constant. A `usize` word changes the lane width at each
  level, which changes the capacity and the depth. This doubles the test matrix
  for no gain.
- The selector policies stay simple. The tournament in Section 2.7 splits a
  word in half at each step. A 32-lane word needs a second version.

A 32-bit target still works correctly, but it works slowly. State this limit in
the rustdoc of the builder method.

**The layout.** The C++ code computes the layout at compile time with
`node_traits<Level>`. Rust cannot size an array from an associated constant
without an unstable feature. Use this instead:

```rust
trait Layout {
    const DEPTH: usize;
    const CAPACITY: usize;
    const NODE_COUNT: usize;
    const LANE_WIDTH: [usize; MAX_DEPTH];
}

struct SignalTree<L: Layout> {
    nodes: Box<[CachePadded<AtomicU64>]>,  // length L::NODE_COUNT
    _layout: PhantomData<L>,
}
```

`DEPTH` is a constant, so the compiler unrolls the descent loop.

**Cache padding.** Use `crate::util::cacheline::CachePadded`. It is already
`repr(align(128))` on the targets that need 128 bytes.

**Loom.** Tokio uses loom 0.7. Loom provides `AtomicU64`. Loom explores every
interleaving, so the tree must be very small in a loom test. Add a loom layout
with 4 signals and depth 1. `queue.rs` already does this: it uses 256 slots
normally and 4 slots under loom.

**Memory order.** Keep the orders of the reference code:

- `set`: `Release` on the leaf `fetch_or` and on each branch `fetch_add`.
- `select`: `Relaxed` on each load, `AcqRel` on the leaf `fetch_and` and on each
  branch compare-and-swap.

Take care with the wake path. Section 3.5 shows that the current code needs a
`SeqCst` read-modify-write. Prove the new path with a loom test before you trust
it. A lost wakeup is the worst failure mode of a scheduler.

---

## 6. The Work Phases

### Phase 0: The baseline

**Goal:** know the numbers before you change anything.

1. Choose the benchmark machine. Write down the core count, the model and the
   kernel version.
2. Run the full benchmark set from Section 8 on `master`.
3. Save the Criterion output as the baseline.
4. Build with `--cfg tokio_internal_mt_counters` and record the counters.

**Exit test:** a saved baseline with the variance for each benchmark.

### Phase 1: The signal tree module alone

**Goal:** a correct and fast data structure, with no Tokio integration.

New files:

```
tokio/src/util/signal_tree/mod.rs           # SignalTree, SignalSet
tokio/src/util/signal_tree/layout.rs        # The Layout trait and the depths
tokio/src/util/signal_tree/selector.rs      # Fairness and densest-child
tokio/src/util/signal_tree/tests.rs         # The unit tests
```

Work items:

1. Write `Layout` for depth 0 to depth 3, plus a loom layout.
2. Write `set` and `select`.
3. Write the two selector policies.
4. Write `SignalSet`: several trees, a scan in turn, and the blocking state.
5. Write the unit tests of Section 7.1.
6. Write the loom tests of Section 7.2.
7. Write the microbenchmarks of Section 8.1.

**Exit test:** all unit tests pass. All loom tests pass. The microbenchmarks
show that `select` costs less than 50 ns with no contention.

**This phase has no risk to the runtime.** The module is dead code until
Phase 3.

### Phase 2: The flavor skeleton

**Goal:** a new flavor that is an exact copy of `multi_thread`, and that passes
the whole test suite.

1. Copy `scheduler/multi_thread/` to `scheduler/multi_thread_signal/`.
2. Make every change point in the table of Section 4.2.
3. Add `Builder::new_multi_thread_signal()` under `cfg_unstable!`.
4. Add the `TOKIO_UNSTABLE_SIGNAL_SCHEDULER` variable of Section 7.6. This one
   step runs the whole test suite against the new flavor. Change no test file.
5. Copy the loom tests to `tokio/src/runtime/tests/loom_multi_thread_signal/`.
   The loom tests call the internals, so step 4 does not reach them.
6. Add the loom job and the label `R-loom-multi-thread-signal` to
   `.github/workflows/loom.yml`.
7. Add the full-suite job of Section 7.8 to `.github/workflows/ci.yml`.

**Exit test:** the command of Section 7.6 passes with no test change. The
benchmarks of the new flavor match `master` within the noise. This proves that
the wiring is correct.

Step 4 is the highest-value step in the plan. Do it before step 5. A copy of
`multi_thread` must pass the full suite, so a failure at this point is a wiring
fault, not a design fault. You find it in minutes, not in days.

### Phase 3: Answer A, the source-level tree

**Goal:** replace `Idle` and the probe.

1. Add a `SignalSet` to `Shared`. Size it for `num_workers + 2` signals.
2. Delete `idle.rs`. Replace `Idle` with a thin type over the signal set. Keep
   the counts that the metrics API needs.
3. Change `queue.rs`: `push_back` must report the empty-to-non-empty change. The
   queue holds a head and a tail already, so this needs no new atomic.
4. Change `Handle::push_remote_task` to set signal *N*.
5. Change `Handle::schedule_local` to set signal *i*.
6. Change `Core::steal_work` to one `select` call and one targeted steal.
7. Add the re-signal step after a steal.
8. Change `Core::transition_to_parked` and `transition_from_parked` to use the
   blocking state of the signal set.
9. Delete `notify_if_work_pending`.

**Exit test:** every test of Phase 2 still passes. The loom tests pass with
`LOOM_MAX_PREEMPTIONS=3`. The benchmarks run.

### Phase 4: The decision gate

Compare Phase 3 with the Phase 0 baseline. Section 8.4 gives the rules.

- **If the gate passes:** go to Phase 5 and to Phase 6.
- **If the gate fails:** write the measurements in this file and stop. Do not go
  to Phase 5. The alternative scheduler of 2023 failed at this gate, and the
  project paid two years of maintenance before it accepted the result.

### Phase 5: Answer B, the task-level tree (gated)

Only start this phase after Phase 4 passes.

1. Add the chunked slot table.
2. Add the free-slot allocator.
3. Add a second mode to the flavor, behind a builder option.
4. Add the Miri tests of Section 7.5 for the unsafe code.
5. Repeat the full benchmark set.

### Phase 6: Polish and metrics

1. Add a selector option to the builder for priority.
2. Export the counters of the signal tree through the unstable metrics API.
3. Write the rustdoc for the new builder method. Say that the flavor is **not**
   production ready, as the old method did. Also say that the flavor aims at a
   64-bit target, and that a 32-bit target is correct but slow.
4. Add a section to `tokio/CHANGELOG.md`.
5. Credit Michael Maniscalco and the `work_contract` project in the module
   documentation.

---

## 7. The Test Plan

### 7.1 Unit tests of the structure

Put these in `tokio/src/util/signal_tree/tests.rs`.

| Test | What it checks |
|---|---|
| `set_then_select` | A set signal comes back from `select`. |
| `select_empty` | `select` on an empty tree returns no signal. |
| `set_is_idempotent` | Two sets of one id give one select. |
| `fill_and_drain` | Set every id, then select every id exactly once. |
| `capacity_bounds` | An id above the capacity is rejected. |
| `root_empty_flag` | The empty flag is true only on the last select. |
| `set_in_empty_tree_flag` | The flag is true only on the first set. |
| `hint_advances` | The hint moves forward and wraps. |
| `wrap_policy` | A no-wrap tree stops at the end. A wrap tree continues. |
| `selector_fairness` | The fair selector spreads the service over the ids. |
| `selector_densest` | The dense selector goes to the fuller subtree. |
| `layout_table` | The computed capacity matches the table of Section 2.3. |

### 7.2 Model test

Write one property test with `proptest`:

- Build a small tree, depth 1.
- Apply a random list of `set` and `select` operations.
- Keep a `BTreeSet<u64>` as the model.
- After each operation, check two rules:
  1. Every selected id was in the model.
  2. The model is empty exactly when the root reports empty.

This test finds faults that fixed cases miss. Run it with 10000 cases in CI.

### 7.3 Loom tests of the structure

Put these in `tokio/src/util/signal_tree/loom_tests.rs`. Use the loom layout,
which holds 4 signals.

| Test | Threads | What it checks |
|---|---|---|
| `concurrent_set_select` | 1 producer, 1 consumer | No lost signal. |
| `two_producers_one_consumer` | 2 + 1 | Each id comes back once. |
| `two_consumers_one_producer` | 1 + 2 | Two consumers never claim one id. |
| `no_lost_wakeup` | 1 + 1 parked | The producer always wakes the consumer. |
| `empty_flag_race` | 2 + 2 | Exactly one thread sees the empty-to-non-empty change. |

`no_lost_wakeup` is the most important test in this plan. A lost wakeup makes a
runtime hang. Run it with `LOOM_MAX_PREEMPTIONS=3`.

### 7.4 Loom tests of the scheduler

Copy `tokio/src/runtime/tests/loom_multi_thread/` to
`loom_multi_thread_signal/`. The groups are `group_a` to `group_d`. Add a job to
`.github/workflows/loom.yml` with the same four-group matrix that the removed
flavor used.

### 7.5 Miri tests

Phase 5 adds unsafe code for the slot table. Run Miri on:

- The slot allocation and the slot release.
- The growth of the chunked table while a reader reads.
- The generation check.

Use the existing Miri setup. `tokio/src/runtime/tests/mod.rs` already has a
`#[cfg(miri)]` branch.

### 7.6 Integration tests: run the whole suite with no test change

Do **not** copy the test files. Tokio has a better method already, and the
project uses it today for the sharded blocking queue.

**The method: an environment variable that changes the default flavor.**

`tokio/src/runtime/builder.rs:253` has this function:

```rust
fn sharded_blocking_queue_default() -> bool {
    match std::env::var_os("TOKIO_UNSTABLE_SHARDED_BLOCKING_QUEUE") {
        Some(value) => !value.is_empty() && value != "0",
        None => false,
    }
}
```

`.github/workflows/ci.yml:338` then runs the **complete** suite with that
variable set:

```yaml
- name: test tokio full with sharded blocking queue
  run: cargo nextest run --features full
  env:
    TOKIO_UNSTABLE_SHARDED_BLOCKING_QUEUE: "1"
```

Copy this pattern. Add `TOKIO_UNSTABLE_SIGNAL_SCHEDULER`. Read it in
`Builder::new_multi_thread()`:

```rust
pub fn new_multi_thread() -> Builder {
    #[cfg(tokio_unstable)]
    if signal_scheduler_default() {
        return Builder::new(Kind::MultiThreadSignal, 61);
    }
    Builder::new(Kind::MultiThread, 61)
}
```

**Why this covers the whole suite.** Every multi-thread test goes through
`Builder::new_multi_thread()`. There are three routes, and all three end there:

| Route | Where it ends | Count in `tokio/tests/` |
|---|---|---|
| `#[tokio::test(flavor = "multi_thread")]` | `tokio-macros/src/entry.rs:475` emits `Builder::new_multi_thread()` | 31 |
| A direct `Builder::new_multi_thread()` call | — | 90 in 30 files |
| `Runtime::new()` | `runtime.rs:180` calls `Builder::new_multi_thread()` | 18 |

The `rt_test!` macro in `tokio/tests/rt_common.rs` also uses
`Builder::new_multi_thread()`. Its two multi-thread modules therefore change
flavor with no edit. That macro runs in `rt_common.rs`,
`rt_handle_block_on.rs` and `task_yield_now.rs`.

The doc tests use the same constructors, so they change flavor too.

**One command then runs everything:**

```bash
RUSTFLAGS="--cfg tokio_unstable" \
TOKIO_UNSTABLE_SIGNAL_SCHEDULER=1 \
cargo nextest run --features full
```

**Why no test asserts the flavor.** A search of `tokio/tests/` and `tokio/src/`
for `runtime_flavor()` and for `RuntimeFlavor::MultiThread` gives no result. No
test reads the flavor, so no test fails because the flavor changed. Keep this
true: make `runtime_flavor()` report the true flavor. Do not make it lie.

**The known exceptions.** Two tests read the internals of a work-stealing
scheduler:

- `rt_unstable_metrics.rs:271`, `worker_steal_count`
- `rt_unstable_metrics.rs:648`, `worker_local_queue_depth`

Answer A keeps the local queues and the stealing, so both tests pass. Answer B
removes them, so both tests fail. This failure is correct, and it is a useful
signal. If you reach Phase 5, gate these two tests on the mode.

**Still copy the loom tests.** The loom tests call the scheduler internals
directly. They do not go through `Builder`, so the variable does not reach them.
Section 7.4 covers them.

**The stress tests.** Run `stress-test/` with the same variable set.

### 7.7 Targeted tests for the new risks

Write these new tests. They cover the faults that the design can cause.

1. **The park race.** Spawn tasks from outside the runtime while all workers
   park and unpark. Run for 10 seconds. Check that the task count is correct.
   This test finds a lost wakeup.
2. **The re-signal rule.** Fill one worker queue. Let a second worker steal one
   task. Check that the signal of the first worker is still set.
3. **Starvation.** Spawn one slow task and many fast tasks. Check that the slow
   task makes progress. The tree is not FIFO, so this risk is real.
4. **Shutdown with set signals.** Shut the runtime down while many signals are
   set. Check that no task leaks and that no thread hangs.
5. **`block_in_place` with a signal tree.** The core moves to another thread.
   Check that the signal of that worker is still correct.
6. **Many workers.** Build a runtime with 64 workers on a machine with fewer
   cores. Check that the tree capacity is correct.

### 7.8 CI

| Job | Command |
|---|---|
| Tests, the default flavor | `cargo nextest run --features full` with `RUSTFLAGS=--cfg tokio_unstable` |
| Tests, the signal flavor | The same command, plus `TOKIO_UNSTABLE_SIGNAL_SCHEDULER=1` |
| Loom, the structure | `cargo test --lib --release --features full -- loom_signal_tree` |
| Loom, the scheduler | Four groups, as in Section 7.4 |
| Miri | The existing Miri job, plus the slot table tests |
| 32-bit | A `i686-unknown-linux-gnu` build and test run. It checks correctness with the mutex shim. It is not a performance test. |

---

## 8. The Benchmark Plan

### 8.1 Microbenchmarks of the structure

Add `benches/signal_tree.rs` and a `[[bench]]` entry in `benches/Cargo.toml`.
These benchmarks copy the reference suite in `work_contract`.

| Benchmark | Measurement |
|---|---|
| `set_uncontended` | The cost of one `set` on one thread. |
| `select_uncontended` | The cost of one `select` on one thread. |
| `set_select_pairs` | The throughput of *T* threads that set and select. |
| `throughput_by_threads` | The same, for *T* = 1, 2, 4, 8, 16, 32, 64. |
| `throughput_by_depth` | The same, for depth 0 to depth 3. |
| `adversarial` | All threads set and select on a few ids. Maximum contention. |
| `service_distance` | How many other signals the tree serves before a given signal. This measures fairness. |
| `sparse_vs_dense` | A full tree against a tree with few set signals. |

Compare each result against a `crossbeam` queue and against a mutex and a
`VecDeque`. These two give the floor and the ceiling.

### 8.2 Scheduler benchmarks

`benches/rt_multi_threaded.rs` holds seven benchmarks today:

| Benchmark | What it stresses |
|---|---|
| `spawn_many_local` | The local queue and the LIFO slot |
| `spawn_many_remote_idle` | The injection queue and the wake path |
| `spawn_many_remote_busy1` | The wake path while the workers are busy |
| `spawn_many_remote_busy2` | The same, with a longer stall |
| `yield_many` | The yield path and the queue order |
| `ping_pong` | The latency of a wake between two tasks |
| `chained_spawn` | The spawn path from inside a task |

Add a flavor parameter to each benchmark. Run both flavors in one Criterion
group, so that Criterion gives a direct comparison.

`spawn_many_remote_idle` and `spawn_many_remote_busy1` are the most important.
They test the wake path, and that path is where the signal tree should win.

Also run:

- `benches/rt_current_thread.rs`: it must not change.
- `benches/spawn.rs`, `benches/sync_mpsc.rs`, `benches/sync_notify.rs`.
- `benches/net_accept.rs`: a real I/O workload.

### 8.3 Macro benchmarks

A microbenchmark can hide a real loss. Run two real workloads:

1. **An HTTP echo server.** Use `hyper` with the new flavor. Drive it with
   `wrk` or with `oha`. Measure the requests for each second and the latency at
   p50, p99 and p99.9.
2. **A message pipeline.** Build a chain of 1000 tasks that pass a value with
   `tokio::sync::mpsc`. Measure the end-to-end latency.

### 8.4 The decision gate

The removal of the old alternative scheduler teaches one lesson: set the targets
first. These are the targets for Phase 4.

**The gate passes when all of these are true:**

| Condition | Target |
|---|---|
| `spawn_many_remote_idle`, 16 workers | 15% or more throughput gain |
| `spawn_many_remote_busy1`, 16 workers | 10% or more throughput gain |
| `ping_pong` | No loss |
| `spawn_many_local` | A loss of 3% or less |
| `chained_spawn` | A loss of 3% or less |
| The HTTP echo server, p99.9 | No loss |
| 32 and 64 workers | The gain does not fall below the gain at 16 workers |
| `rt_current_thread` | No change |

The last two rows matter most. The old flavor "does not scale as well to 16+
processors". A new flavor that repeats this result gives no value.

### 8.5 Method

A bad measurement is worse than no measurement. Follow these rules:

1. Use one machine for all runs. Write down the model and the core count.
2. Pin the threads. Use `taskset`. Isolate the cores with `isolcpus` if you can.
3. Turn the turbo mode off. Set the CPU governor to `performance`.
4. Run each benchmark five times. Report the median and the range.
5. Use `criterion --save-baseline` and `--baseline` for the comparison.
6. Collect the hardware counters with `perf stat`:
   - `context-switches` and `cpu-migrations` (the wake cost),
   - `cache-misses` and `LLC-load-misses` (the cost of the shared lines),
   - `futex` calls through `strace -c -f` (the system call count).
7. Build with `--cfg tokio_internal_mt_counters` for the internal counts.
8. Measure at 1, 2, 4, 8, 16, 32 and 64 workers. Plot the result. A scheduler
   plan lives or dies on the shape of this curve.

### 8.6 The counters to add

Add these to `counters.rs` for the new flavor:

- `num_signal_set` and `num_signal_set_in_empty_tree`.
- `num_signal_select` and `num_signal_select_empty`.
- `num_cas_retries` in `select`.
- `num_resignal_after_steal`.
- `select_depth_histogram`.

The ratio of `num_signal_set_in_empty_tree` to `num_signal_set` gives the wake
rate. A low ratio proves the main claim of the design.

---

## 9. Risks

| Risk | Effect | Action |
|---|---|---|
| A lost wakeup | The runtime hangs | The loom test `no_lost_wakeup` (7.3), and the park race test (7.7.1) |
| The order is not FIFO | A task starves | The starvation test (7.7.3). Keep `global_queue_interval`. |
| A 32-bit target has no 64-bit atomic | A mutex replaces the atomic. The flavor is slow on that target. | Accepted (5.5). Keep the `i686` CI job for correctness only. State the limit in the rustdoc. |
| The loom state space grows too large | The loom job times out | A 4-signal loom layout. Four test groups. |
| The capacity is fixed | A runtime with many workers fails | `SignalSet` adds trees. Test with 64 workers (7.7.6). |
| The gain does not hold at 32+ cores | The flavor repeats the 2023 result | The gate at 8.4. Measure early, at Phase 4. |
| The copy of the scheduler is expensive to maintain | The code rots | Share `queue.rs`, `park.rs` and `stats.rs`. Copy only `worker.rs`. |
| The task state and the leaf bit disagree | A task runs twice or never | Phase 5 only. Make the leaf bit the authority (5.4). |

---

## 10. The Files to Change

### New files

```
tokio/src/util/signal_tree/mod.rs
tokio/src/util/signal_tree/layout.rs
tokio/src/util/signal_tree/selector.rs
tokio/src/util/signal_tree/tests.rs
tokio/src/util/signal_tree/loom_tests.rs
tokio/src/runtime/scheduler/multi_thread_signal/*        (from multi_thread)
tokio/src/runtime/tests/loom_multi_thread_signal.rs
tokio/src/runtime/tests/loom_multi_thread_signal/*
benches/signal_tree.rs
```

**No new test file in `tokio/tests/`.** Section 7.6 gives the reason.

### Changed files

```
tokio/src/util/mod.rs                       # Add the module
tokio/src/runtime/scheduler/mod.rs          # Handle, Context, match_flavor!
tokio/src/runtime/builder.rs                # Kind, the constructor, the build method,
                                            # TOKIO_UNSTABLE_SIGNAL_SCHEDULER
tokio/src/runtime/runtime.rs                # RuntimeFlavor, Scheduler, block_on, Drop
tokio/src/runtime/handle.rs                 # runtime_flavor, owned_id, the task dump
tokio/src/runtime/blocking/schedule.rs      # Two match arms
tokio/src/runtime/scheduler/block_in_place.rs
tokio/src/runtime/tests/mod.rs              # The loom module
benches/Cargo.toml                          # The bench entry
benches/rt_multi_threaded.rs                # The flavor parameter
.github/workflows/ci.yml                    # The full-suite job
.github/workflows/loom.yml                  # The loom job and the label
tokio/CHANGELOG.md
```

---

## 11. References

1. Michael Maniscalco. "Signal Trees". C++Now 2026.
   <https://schedule.cppnow.org/session/2026/signal-trees/>
   Slides: <https://schedule.cppnow.org/wp-content/uploads/2026/03/signal_trees_cnow_2026.pdf>
2. The reference code: <https://github.com/buildingcpp/work_contract>
   Read `src/library/signal_tree/signal_tree.h` first. It holds the full
   algorithm in 409 lines.
3. Carl Lerche. "Making the Tokio scheduler 10x faster". 2019.
   <https://tokio.rs/blog/2019-10-scheduler>
4. The removal of the alternative scheduler: commit `159a3b2c`, PR #7275.
5. The cleanup of the alternative tests: commit `7dd4d8a3`, PR #7451.
6. A related design for comparison: "Work-Stealing, Locality-Aware Actor
   Scheduling". IPDPS 2018.
   <https://cs.uwaterloo.ca/~mkarsten/papers/ipdps2018-preprint.pdf>
