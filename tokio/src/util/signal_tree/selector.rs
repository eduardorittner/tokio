//! The policies that pick a lane in a branch word and a bit in a leaf word.
//!
//! The structure does not set the service order. This policy does. A new
//! policy is a new unit struct with an impl of [`Selector`].

/// Picks one child of a node.
pub(crate) trait Selector {
    /// Returns the index of a lane of `word` that holds a count above zero.
    ///
    /// The caller guarantees that at least one lane holds a count above zero.
    /// `bias` is the lane that the caller prefers; it comes from the hint of
    /// the worker.
    fn select_lane(word: u64, fanout: usize, width: usize, bias: usize) -> usize;

    /// Returns the index of a set bit of `word`.
    ///
    /// The caller guarantees that `word` is not zero. `bias` is the bit that
    /// the caller prefers.
    fn select_bit(word: u64, bias: usize) -> usize;
}

/// Reads the counter of lane `lane`.
#[inline]
pub(crate) fn lane(word: u64, lane: usize, width: usize) -> u64 {
    let mask = mask(width);
    (word >> (lane * width)) & mask
}

/// Returns the mask of one lane.
#[inline]
pub(crate) fn mask(width: usize) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Spreads the service over the id space.
///
/// The policy starts at the lane of the hint and moves forward. It wraps at the
/// end. A worker therefore continues near its last position, and two workers
/// with different hints do not collide at the same lane.
pub(crate) struct Fairness;

impl Selector for Fairness {
    #[inline]
    fn select_lane(word: u64, fanout: usize, width: usize, bias: usize) -> usize {
        let start = bias % fanout;

        let mut i = 0;
        while i < fanout {
            let index = {
                let index = start + i;
                if index >= fanout {
                    index - fanout
                } else {
                    index
                }
            };

            if lane(word, index, width) != 0 {
                return index;
            }

            i += 1;
        }

        // The caller guarantees that a lane holds a count above zero, so this
        // point is only reached if the word changed under us. The caller
        // re-reads the word and tries again, so any lane is a safe answer.
        start
    }

    #[inline]
    fn select_bit(word: u64, bias: usize) -> usize {
        debug_assert!(word != 0);

        let bias = bias % 64;

        // The set bits at or above the bias.
        let high = word & !((1u64 << bias) - 1);

        if high != 0 {
            high.trailing_zeros() as usize
        } else {
            word.trailing_zeros() as usize
        }
    }
}

/// Goes to the child that holds the most signals.
///
/// This policy drains a hot subtree first. It gives better locality than
/// [`Fairness`] and worse fairness.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct DensestChild;

impl Selector for DensestChild {
    #[inline]
    fn select_lane(word: u64, fanout: usize, width: usize, _bias: usize) -> usize {
        let mut best = 0;
        let mut best_count = 0;

        let mut index = 0;
        while index < fanout {
            let count = lane(word, index, width);
            if count > best_count {
                best = index;
                best_count = count;
            }
            index += 1;
        }

        best
    }

    #[inline]
    fn select_bit(word: u64, _bias: usize) -> usize {
        debug_assert!(word != 0);
        word.trailing_zeros() as usize
    }
}
