use super::layout::{Depth0, Depth1, Depth2, Depth3, Layout, LoomLayout};
use super::selector::DensestChild;
use super::{SignalSet, SignalTree};

use std::collections::BTreeSet;

#[test]
fn layout_table() {
    // The table of the design document, section 2.3.
    assert_eq!((Depth0::DEPTH, Depth0::CAPACITY, Depth0::NODE_COUNT), (0, 64, 1));
    assert_eq!((Depth1::DEPTH, Depth1::CAPACITY, Depth1::NODE_COUNT), (1, 512, 9));
    assert_eq!((Depth2::DEPTH, Depth2::CAPACITY, Depth2::NODE_COUNT), (2, 2048, 37));
    assert_eq!((Depth3::DEPTH, Depth3::CAPACITY, Depth3::NODE_COUNT), (3, 8192, 149));

    assert_eq!(Depth1::LANE_WIDTH[0], 8);
    assert_eq!(Depth2::LANE_WIDTH[0], 16);
    assert_eq!(Depth2::LANE_WIDTH[1], 8);
    assert_eq!(Depth3::LANE_WIDTH[0], 16);
    assert_eq!(Depth3::LANE_WIDTH[1], 16);
    assert_eq!(Depth3::LANE_WIDTH[2], 8);

    assert_eq!((LoomLayout::DEPTH, LoomLayout::CAPACITY, LoomLayout::NODE_COUNT), (1, 128, 3));
}

#[test]
fn set_then_select() {
    fn case<L: Layout>() {
        let tree = SignalTree::<L>::new();
        let mut hint = 0;

        assert!(tree.set(7));
        assert_eq!(tree.select(&mut hint).map(|s| s.id), Some(7));
    }

    case::<Depth0>();
    case::<Depth1>();
    case::<Depth2>();
    case::<Depth3>();
}

#[test]
fn select_empty() {
    fn case<L: Layout>() {
        let tree = SignalTree::<L>::new();
        let mut hint = 0;

        assert!(tree.is_empty());
        assert!(tree.select(&mut hint).is_none());
    }

    case::<Depth0>();
    case::<Depth1>();
    case::<Depth2>();
    case::<Depth3>();
}

#[test]
fn set_is_idempotent() {
    let tree = SignalTree::<Depth1>::new();
    let mut hint = 0;

    assert!(tree.set(130));
    assert!(!tree.set(130));
    assert!(!tree.set(130));

    assert_eq!(tree.select(&mut hint).map(|s| s.id), Some(130));
    assert!(tree.select(&mut hint).is_none());
}

#[test]
fn fill_and_drain() {
    fn case<L: Layout>() {
        let tree = SignalTree::<L>::new();

        for id in 0..L::CAPACITY {
            assert_eq!(tree.set(id), id == 0, "id {id}");
        }

        let mut seen = BTreeSet::new();
        let mut hint = 0;

        while let Some(selected) = tree.select(&mut hint) {
            assert!(seen.insert(selected.id), "id {} came back twice", selected.id);
            assert_eq!(selected.root_empty, seen.len() == L::CAPACITY);
        }

        assert_eq!(seen.len(), L::CAPACITY);
        assert!(tree.is_empty());
    }

    case::<Depth0>();
    case::<Depth1>();
    case::<Depth2>();
    case::<Depth3>();
}

#[test]
#[should_panic(expected = "signal id out of range")]
fn capacity_bounds() {
    let tree = SignalTree::<Depth1>::new();
    tree.set(Depth1::CAPACITY);
}

#[test]
fn root_empty_flag() {
    let tree = SignalTree::<Depth1>::new();
    let mut hint = 0;

    tree.set(1);
    tree.set(100);
    tree.set(500);

    assert!(!tree.select(&mut hint).unwrap().root_empty);
    assert!(!tree.select(&mut hint).unwrap().root_empty);
    assert!(tree.select(&mut hint).unwrap().root_empty);
}

#[test]
fn set_in_empty_tree_flag() {
    let tree = SignalTree::<Depth2>::new();
    let mut hint = 0;

    assert!(tree.set(3));
    assert!(!tree.set(1000));
    assert!(!tree.set(2047));

    while tree.select(&mut hint).is_some() {}

    // The tree is empty again, so the next set reports the change.
    assert!(tree.set(42));
}

#[test]
fn hint_advances() {
    let tree = SignalTree::<Depth1>::new();
    let mut hint = 0;

    tree.set(10);
    tree.set(400);

    let first = tree.select(&mut hint).unwrap();
    assert_eq!(hint, first.id + 1);

    let second = tree.select(&mut hint).unwrap();
    assert_eq!(hint, second.id + 1);
    assert_ne!(first.id, second.id);
}

#[test]
fn hint_biases_the_walk() {
    let tree = SignalTree::<Depth1>::new();

    tree.set(0);
    tree.set(256);

    // A hint inside the second half picks the signal of the second half.
    let mut hint = 300;
    assert_eq!(tree.select(&mut hint).map(|s| s.id), Some(256));

    // With that signal gone, the walk wraps back to the first half.
    let mut hint = 300;
    assert_eq!(tree.select(&mut hint).map(|s| s.id), Some(0));
}

#[test]
fn selector_fairness() {
    // Every id is set once. A fair walk with a moving hint visits each id
    // exactly once and never stalls on one leaf.
    let tree = SignalTree::<Depth1>::new();

    for id in 0..Depth1::CAPACITY {
        tree.set(id);
    }

    let mut hint = 0;
    let mut order = Vec::new();

    while let Some(selected) = tree.select(&mut hint) {
        order.push(selected.id);
    }

    assert_eq!(order.len(), Depth1::CAPACITY);
    assert_eq!(order, (0..Depth1::CAPACITY).collect::<Vec<_>>());
}

#[test]
fn selector_densest() {
    let tree = SignalTree::<Depth1>::new();

    // Leaf 0 holds one signal. Leaf 1 holds three.
    tree.set(0);
    tree.set(64);
    tree.set(65);
    tree.set(66);

    let mut hint = 0;
    let selected = tree.select_with::<DensestChild>(&mut hint).unwrap();

    // The dense policy goes to the fuller subtree even though the hint points
    // at the first one.
    assert_eq!(selected.id, 64);
}

#[test]
fn signal_set_spans_several_trees() {
    let per_tree = SignalTree::<super::DefaultLayout>::capacity();
    let set = SignalSet::with_capacity(per_tree * 2 + 1);

    assert!(set.capacity() >= per_tree * 2 + 1);
    assert!(set.is_empty());

    let ids = [0, per_tree, per_tree * 2];

    for id in ids {
        set.set(id);
    }

    assert!(!set.is_empty());

    let mut seen = BTreeSet::new();
    let mut hint = 0;

    while let Some(id) = set.select(&mut hint) {
        assert!(seen.insert(id));
    }

    assert_eq!(seen, ids.into_iter().collect::<BTreeSet<_>>());
    assert!(set.is_empty());
}

#[test]
fn signal_set_is_idempotent() {
    let set = SignalSet::with_capacity(8);
    let mut hint = 0;

    assert!(set.set(3));
    assert!(!set.set(3));

    assert_eq!(set.select(&mut hint), Some(3));
    assert_eq!(set.select(&mut hint), None);
}

#[test]
fn concurrent_set_and_select() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::{Arc, Barrier};

    const PRODUCERS: usize = 4;
    const CONSUMERS: usize = 4;
    const PER_PRODUCER: usize = 64;

    let tree = Arc::new(SignalTree::<Depth2>::new());
    let claimed = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(PRODUCERS + CONSUMERS));

    let total = PRODUCERS * PER_PRODUCER;
    let mut threads = Vec::new();

    for p in 0..PRODUCERS {
        let tree = tree.clone();
        let start = start.clone();

        threads.push(std::thread::spawn(move || {
            start.wait();
            for i in 0..PER_PRODUCER {
                tree.set(p * PER_PRODUCER + i);
            }
            BTreeSet::new()
        }));
    }

    for _ in 0..CONSUMERS {
        let tree = tree.clone();
        let claimed = claimed.clone();
        let start = start.clone();

        threads.push(std::thread::spawn(move || {
            start.wait();
            let mut hint = 0;
            let mut seen = BTreeSet::new();

            while claimed.load(Relaxed) < total {
                if let Some(selected) = tree.select(&mut hint) {
                    assert!(seen.insert(selected.id), "id claimed twice by one thread");
                    claimed.fetch_add(1, Relaxed);
                } else {
                    std::thread::yield_now();
                }
            }

            seen
        }));
    }

    let mut all = BTreeSet::new();
    let mut counted = 0;

    for (index, thread) in threads.into_iter().enumerate() {
        let seen = thread.join().unwrap();

        if index >= PRODUCERS {
            counted += seen.len();
            all.extend(seen);
        }
    }

    assert_eq!(counted, all.len(), "an id was claimed by two threads");
    assert_eq!(all.len(), total);
    assert!(tree.is_empty());
}
