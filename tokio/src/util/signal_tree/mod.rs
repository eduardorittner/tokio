//! A concurrent readiness set that selects a ready signal in `O(depth)` steps.
//!
//! # Credit
//!
//! The structure is a port of the `signal_tree` library of Michael Maniscalco,
//! shown in the talk "Signal Trees" at C++Now 2026. The reference code is at
//! <https://github.com/buildingcpp/work_contract> and is under the MIT license.
//!
//! # The structure
//!
//! A signal tree is an array of 64-bit atomic words. Each word is on its own
//! cache line.
//!
//! * A **leaf** word is a bitset of 64 signals. Bit `i` is `1` when signal `i`
//!   is ready.
//! * A **branch** word holds a group of counters. The code calls one counter a
//!   *lane*. Each lane counts the set signals of one child subtree.
//!
//! [`SignalTree::set`] walks from the leaf up to the root and increments one
//! lane at each level. [`SignalTree::select`] walks from the root down to a
//! leaf and decrements one lane at each level. A decrement **reserves** one
//! signal of that subtree, so two workers never claim the same signal and a
//! worker never descends into a subtree that holds no work.
//!
//! # The order of the two walks
//!
//! [`SignalTree::set`] increments from the leaf up. [`SignalTree::select`]
//! decrements from the root down. This order keeps the following rule true for
//! each node `n` with a parent lane `p`:
//!
//! ```text
//! total(n) >= p + reservations(n)
//! ```
//!
//! A worker that holds a reservation therefore always finds `total(n) > 0`. It
//! never spins on an empty subtree.

use crate::loom::sync::atomic::AtomicU64;
use crate::util::cacheline::CachePadded;

use std::marker::PhantomData;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};

pub(crate) mod layout;
pub(crate) mod selector;

use layout::{Layout, LEAF_BITS};
use selector::{Fairness, Selector};

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_signal_tree;

/// The shape that the runtime uses.
#[cfg(not(loom))]
pub(crate) type DefaultLayout = layout::Depth1;

/// A small shape keeps the `loom` state space inside the model checker budget.
#[cfg(loom)]
pub(crate) type DefaultLayout = layout::LoomLayout;

/// The result of [`SignalTree::select`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct Selected {
    /// The id of the claimed signal.
    pub(crate) id: usize,

    /// `true` when this claim emptied the root.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) root_empty: bool,
}

/// A fixed-capacity concurrent readiness set.
pub(crate) struct SignalTree<L: Layout> {
    nodes: Box<[CachePadded<AtomicU64>]>,
    _layout: PhantomData<L>,
}

impl<L: Layout> SignalTree<L> {
    /// Returns a tree with no set signal.
    pub(crate) fn new() -> SignalTree<L> {
        let mut nodes = Vec::with_capacity(L::NODE_COUNT);
        for _ in 0..L::NODE_COUNT {
            nodes.push(CachePadded::new(AtomicU64::new(0)));
        }

        SignalTree {
            nodes: nodes.into_boxed_slice(),
            _layout: PhantomData,
        }
    }

    /// Returns the number of signals that the tree holds.
    pub(crate) const fn capacity() -> usize {
        L::CAPACITY
    }

    /// Makes signal `id` ready.
    ///
    /// Returns `true` when this call changed the tree from empty to not empty.
    /// Only the caller that gets `true` must wake a sleeping worker.
    ///
    /// The call is idempotent: a second set of the same id does nothing and
    /// returns `false`.
    ///
    /// # Panics
    ///
    /// Panics when `id` is at or above the capacity.
    pub(crate) fn set(&self, id: usize) -> bool {
        assert!(id < L::CAPACITY, "signal id out of range");

        let leaf = L::LEVEL_OFFSET[L::DEPTH] + id / LEAF_BITS;
        let bit = 1u64 << (id % LEAF_BITS);

        let prev = self.nodes[leaf].fetch_or(bit, Release);

        if prev & bit != 0 {
            // The signal was already ready.
            return false;
        }

        if L::DEPTH == 0 {
            return prev == 0;
        }

        // Climb to the root. The leaf bit is set first, so a lane counter is
        // never above the number of set bits of its subtree.
        let mut became_non_empty = false;
        let mut level = L::DEPTH;

        while level > 0 {
            level -= 1;

            let node = L::LEVEL_OFFSET[level] + id / L::SUBTREE[level];
            let lane = (id / L::SUBTREE[level + 1]) % L::FANOUT[level];
            let step = 1u64 << (lane * L::LANE_WIDTH[level]);

            let prev = self.nodes[node].fetch_add(step, Release);

            if level == 0 {
                became_non_empty = prev == 0;
            }
        }

        became_non_empty
    }

    /// Claims one ready signal.
    ///
    /// Returns `None` when the tree holds no claimable signal. `hint` is a
    /// cursor into the id space: the call reads it to stay near the last
    /// position of this worker, and writes the next position into it.
    pub(crate) fn select(&self, hint: &mut usize) -> Option<Selected> {
        self.select_with::<Fairness>(hint)
    }

    /// Claims one ready signal with the policy `S`.
    pub(crate) fn select_with<S: Selector>(&self, hint: &mut usize) -> Option<Selected> {
        // The first signal id of the subtree that the walk is in.
        let mut base = 0;
        // The absolute index of the node that the walk is in.
        let mut node = 0;
        let mut root_empty = false;

        let mut level = 0;
        while level < L::DEPTH {
            let fanout = L::FANOUT[level];
            let width = L::LANE_WIDTH[level];
            let bias = (*hint / L::SUBTREE[level + 1]) % fanout;

            let mut current = self.nodes[node].load(Relaxed);

            let lane = loop {
                if current == 0 {
                    // Only the root can be empty here. Below the root the walk
                    // holds a reservation, which keeps the node above zero.
                    debug_assert_eq!(level, 0, "reserved subtree is empty");
                    return None;
                }

                let lane = S::select_lane(current, fanout, width, bias);

                if selector::lane(current, lane, width) == 0 {
                    // The policy answered from a word that changed. Re-read.
                    current = self.nodes[node].load(Relaxed);
                    continue;
                }

                let next = current - (1u64 << (lane * width));

                match self.nodes[node].compare_exchange_weak(current, next, AcqRel, Relaxed) {
                    Ok(_) => {
                        if level == 0 {
                            root_empty = next == 0;
                        }
                        break lane;
                    }
                    Err(actual) => current = actual,
                }
            };

            base += lane * L::SUBTREE[level + 1];
            node = L::LEVEL_OFFSET[level + 1] + base / L::SUBTREE[level + 1];
            level += 1;
        }

        // At the leaf. A reservation guarantees a set bit, so the only reason
        // to go around this loop again is that another worker took the bit
        // that the policy picked.
        loop {
            let word = self.nodes[node].load(Relaxed);

            if word == 0 {
                if L::DEPTH == 0 {
                    return None;
                }

                debug_assert!(false, "reserved leaf is empty");
                continue;
            }

            let index = S::select_bit(word, *hint % LEAF_BITS);
            let bit = 1u64 << index;

            let prev = self.nodes[node].fetch_and(!bit, AcqRel);

            if prev & bit != 0 {
                let id = base + index;

                if L::DEPTH == 0 {
                    root_empty = prev & !bit == 0;
                }

                *hint = id + 1;

                return Some(Selected { id, root_empty });
            }
        }
    }

    /// Returns `true` when the tree holds no claimable signal.
    ///
    /// A set that is in flight may be invisible to this call. The caller pairs
    /// the call with a `SeqCst` fence when it must not miss a set.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes[0].load(Acquire) == 0
    }
}

impl<L: Layout> Default for SignalTree<L> {
    fn default() -> SignalTree<L> {
        SignalTree::new()
    }
}

/// One or more [`SignalTree`]s that together hold any number of signals.
///
/// A single tree has a fixed capacity. A set grows by adding trees and reading
/// them in turn.
pub(crate) struct SignalSet {
    trees: Box<[SignalTree<DefaultLayout>]>,
}

impl SignalSet {
    /// Returns a set that holds at least `capacity` signals.
    pub(crate) fn with_capacity(capacity: usize) -> SignalSet {
        let per_tree = SignalTree::<DefaultLayout>::capacity();
        let count = capacity.div_ceil(per_tree).max(1);

        let mut trees = Vec::with_capacity(count);
        for _ in 0..count {
            trees.push(SignalTree::new());
        }

        SignalSet {
            trees: trees.into_boxed_slice(),
        }
    }

    /// Returns the number of signals that the set holds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn capacity(&self) -> usize {
        self.trees.len() * SignalTree::<DefaultLayout>::capacity()
    }

    /// Makes signal `id` ready.
    ///
    /// Returns `true` when this call changed the tree that holds `id` from
    /// empty to not empty. With more than one tree the answer is a safe
    /// over-estimate: another tree may hold a signal.
    pub(crate) fn set(&self, id: usize) -> bool {
        let per_tree = SignalTree::<DefaultLayout>::capacity();
        self.trees[id / per_tree].set(id % per_tree)
    }

    /// Claims one ready signal.
    ///
    /// The scan starts at the tree of the hint, so a worker stays near its last
    /// position.
    pub(crate) fn select(&self, hint: &mut usize) -> Option<usize> {
        let per_tree = SignalTree::<DefaultLayout>::capacity();
        let count = self.trees.len();
        let start = (*hint / per_tree) % count;

        for offset in 0..count {
            let index = (start + offset) % count;

            // Each tree keeps its own cursor, so move the hint into the id
            // space of the tree that this step reads.
            let mut local = *hint % per_tree;

            if let Some(selected) = self.trees[index].select(&mut local) {
                let id = index * per_tree + selected.id;
                *hint = id + 1;
                return Some(id);
            }
        }

        None
    }

    /// Returns `true` when no tree holds a claimable signal.
    ///
    /// A set that is in flight may be invisible to this call. The caller pairs
    /// the call with a `SeqCst` fence when it must not miss a set.
    pub(crate) fn is_empty(&self) -> bool {
        self.trees.iter().all(SignalTree::is_empty)
    }
}
