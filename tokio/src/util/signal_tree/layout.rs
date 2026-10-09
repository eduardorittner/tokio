//! Compile-time shapes for a [`SignalTree`].
//!
//! [`SignalTree`]: super::SignalTree

/// The largest number of branch levels that a tree can have.
pub(crate) const MAX_DEPTH: usize = 3;

/// The number of signals that one leaf word holds.
pub(crate) const LEAF_BITS: usize = 64;

/// The number of bits in one node word.
pub(crate) const WORD_BITS: usize = 64;

/// All of the derived numbers for one tree shape.
///
/// Level `0` is the root. Level `DEPTH` holds the leaves. A tree of depth `0`
/// is a single leaf word.
pub(crate) struct Shape {
    /// The number of branch levels above the leaves.
    pub(crate) depth: usize,

    /// The number of signals that the tree holds.
    pub(crate) capacity: usize,

    /// The number of atomic words that the tree holds.
    pub(crate) node_count: usize,

    /// The number of children of one node at each branch level.
    pub(crate) fanout: [usize; MAX_DEPTH],

    /// The width in bits of one lane counter at each branch level.
    pub(crate) lane_width: [usize; MAX_DEPTH],

    /// The index of the first node of each level.
    pub(crate) level_offset: [usize; MAX_DEPTH + 1],

    /// The number of signals below one node at each level.
    pub(crate) subtree: [usize; MAX_DEPTH + 1],
}

/// Returns the lane width for a subtree that holds `capacity` signals.
///
/// A lane counts from `0` to `capacity`, so it needs `b` bits where
/// `2^b > capacity`. The width is the next power of two above `b`, which keeps
/// the lane extraction a shift and a mask.
const fn lane_width_for(capacity: usize) -> usize {
    let mut bits = 1;
    while (1usize << bits) <= capacity {
        bits += 1;
    }

    let mut width = 1;
    while width < bits {
        width *= 2;
    }

    width
}

/// Computes a [`Shape`] from a depth and a fan-out list.
///
/// Entries of `fanout` at an index of `depth` or above are not used.
pub(crate) const fn shape(depth: usize, fanout: [usize; MAX_DEPTH]) -> Shape {
    assert!(depth <= MAX_DEPTH);

    // The number of signals below one node, from the leaves up.
    let mut subtree = [0; MAX_DEPTH + 1];
    subtree[depth] = LEAF_BITS;

    let mut level = depth;
    while level > 0 {
        level -= 1;
        subtree[level] = fanout[level] * subtree[level + 1];
    }

    // The index of the first node of each level, and the total node count.
    let mut level_offset = [0; MAX_DEPTH + 1];
    let mut offset = 0;
    let mut level_size = 1;
    let mut level = 0;
    while level <= depth {
        level_offset[level] = offset;
        offset += level_size;
        if level < depth {
            level_size *= fanout[level];
        }
        level += 1;
    }

    // The lane width of each branch level.
    let mut lane_width = [0; MAX_DEPTH];
    let mut level = 0;
    while level < depth {
        lane_width[level] = lane_width_for(subtree[level + 1]);
        // All of the lanes of one level must fit in one word.
        assert!(fanout[level] * lane_width[level] <= WORD_BITS);
        level += 1;
    }

    Shape {
        depth,
        capacity: subtree[0],
        node_count: offset,
        fanout,
        lane_width,
        level_offset,
        subtree,
    }
}

/// The shape of a tree, as a set of associated constants.
///
/// The constants let the compiler unroll the descent loop of
/// [`SignalTree::select`] and the climb loop of [`SignalTree::set`].
///
/// [`SignalTree::select`]: super::SignalTree::select
/// [`SignalTree::set`]: super::SignalTree::set
pub(crate) trait Layout {
    const DEPTH: usize;
    const CAPACITY: usize;
    const NODE_COUNT: usize;
    const FANOUT: [usize; MAX_DEPTH];
    const LANE_WIDTH: [usize; MAX_DEPTH];
    const LEVEL_OFFSET: [usize; MAX_DEPTH + 1];
    const SUBTREE: [usize; MAX_DEPTH + 1];
}

macro_rules! declare_layout {
    ($(#[$attr:meta])* $name:ident, $depth:expr, $fanout:expr) => {
        $(#[$attr])*
        #[cfg_attr(not(test), allow(dead_code))]
        pub(crate) struct $name;

        impl $name {
            #[cfg_attr(not(test), allow(dead_code))]
            const SHAPE: Shape = shape($depth, $fanout);
        }

        impl Layout for $name {
            const DEPTH: usize = Self::SHAPE.depth;
            const CAPACITY: usize = Self::SHAPE.capacity;
            const NODE_COUNT: usize = Self::SHAPE.node_count;
            const FANOUT: [usize; MAX_DEPTH] = Self::SHAPE.fanout;
            const LANE_WIDTH: [usize; MAX_DEPTH] = Self::SHAPE.lane_width;
            const LEVEL_OFFSET: [usize; MAX_DEPTH + 1] = Self::SHAPE.level_offset;
            const SUBTREE: [usize; MAX_DEPTH + 1] = Self::SHAPE.subtree;
        }
    };
}

declare_layout!(
    /// One leaf word: 64 signals in 1 word.
    Depth0,
    0,
    [0, 0, 0]
);

declare_layout!(
    /// One root and eight leaves: 512 signals in 9 words.
    Depth1,
    1,
    [8, 0, 0]
);

declare_layout!(
    /// 2048 signals in 37 words.
    Depth2,
    2,
    [4, 8, 0]
);

declare_layout!(
    /// 8192 signals in 149 words.
    Depth3,
    3,
    [4, 4, 8]
);

declare_layout!(
    /// A small shape for `loom`: one root, two leaves, three words.
    ///
    /// The capacity stays at 128 because a leaf word always holds 64 signals.
    /// A `loom` test uses only a few ids, so only the root and one or two leaf
    /// words are ever touched.
    LoomLayout,
    1,
    [2, 0, 0]
);
