//! `loom` tests of the signal tree.
//!
//! These tests use [`LoomLayout`], which has one root and two leaves. The ids
//! `0` and `64` sit in different leaves, so a test that uses both exercises the
//! full walk with the smallest possible state space.
//!
//! [`LoomLayout`]: super::layout::LoomLayout

use super::{SignalSet, SignalTree};

use loom::sync::atomic::AtomicUsize;
use loom::sync::Arc;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};

type Tree = SignalTree<super::DefaultLayout>;

/// Claims signals until the tree gives nothing back.
fn drain(tree: &Tree) -> BTreeSet<usize> {
    let mut hint = 0;
    let mut out = BTreeSet::new();

    while let Some(selected) = tree.select(&mut hint) {
        assert!(out.insert(selected.id), "id {} came back twice", selected.id);
    }

    out
}

#[test]
fn concurrent_set_select() {
    loom::model(|| {
        let tree = Arc::new(Tree::new());

        let producer = {
            let tree = tree.clone();
            thread::spawn(move || {
                tree.set(0);
            })
        };

        let consumer = {
            let tree = tree.clone();
            thread::spawn(move || drain(&tree))
        };

        producer.join().unwrap();
        let mut claimed = consumer.join().unwrap();

        // The consumer may run before the producer. Whatever it left behind
        // must still be in the tree.
        claimed.extend(drain(&tree));

        assert_eq!(claimed, [0].into_iter().collect::<BTreeSet<_>>());
    });
}

#[test]
fn two_producers_one_consumer() {
    loom::model(|| {
        let tree = Arc::new(Tree::new());

        let p1 = {
            let tree = tree.clone();
            thread::spawn(move || {
                tree.set(0);
            })
        };

        let p2 = {
            let tree = tree.clone();
            thread::spawn(move || {
                // A second leaf, so the walk must pick a lane.
                tree.set(64);
            })
        };

        let consumer = {
            let tree = tree.clone();
            thread::spawn(move || drain(&tree))
        };

        p1.join().unwrap();
        p2.join().unwrap();
        let mut claimed = consumer.join().unwrap();

        claimed.extend(drain(&tree));

        assert_eq!(claimed, [0, 64].into_iter().collect::<BTreeSet<_>>());
        assert!(tree.is_empty());
    });
}

#[test]
fn two_consumers_one_producer() {
    loom::model(|| {
        let tree = Arc::new(Tree::new());

        tree.set(0);
        tree.set(64);

        let c1 = {
            let tree = tree.clone();
            thread::spawn(move || drain(&tree))
        };

        let c2 = {
            let tree = tree.clone();
            thread::spawn(move || drain(&tree))
        };

        let first = c1.join().unwrap();
        let second = c2.join().unwrap();

        // No id may reach two consumers.
        assert!(
            first.is_disjoint(&second),
            "an id was claimed by two consumers"
        );

        let mut all = first;
        all.extend(second);

        assert_eq!(all, [0, 64].into_iter().collect::<BTreeSet<_>>());
        assert!(tree.is_empty());
    });
}

#[test]
fn empty_flag_race() {
    loom::model(|| {
        let tree = Arc::new(Tree::new());
        let flags = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..2)
            .map(|i| {
                let tree = tree.clone();
                let flags = flags.clone();

                thread::spawn(move || {
                    if tree.set(i * 64) {
                        flags.fetch_add(1, Relaxed);
                    }
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        // Exactly one producer may see the change from empty to not empty.
        assert_eq!(flags.load(Relaxed), 1);
        assert_eq!(drain(&tree), [0, 64].into_iter().collect::<BTreeSet<_>>());
    });
}

/// The park protocol of the scheduler, in the smallest form that can lose a
/// wakeup.
///
/// The worker publishes that it sleeps and then reads the work set. The
/// producer publishes work and then reads the sleeper set. A `SeqCst` fence
/// sits between the write and the read on both sides, so at most one of the two
/// reads can miss the write of the other side.
///
/// A failure of this test is a runtime that hangs.
#[test]
fn no_lost_wakeup() {
    loom::model(|| {
        let work = Arc::new(SignalSet::with_capacity(1));
        let sleepers = Arc::new(SignalSet::with_capacity(1));

        // Set by the worker when it would go to sleep.
        let went_to_sleep = Arc::new(AtomicUsize::new(0));
        // Set by the producer when it found a sleeper to wake.
        let woke_someone = Arc::new(AtomicUsize::new(0));

        let producer = {
            let work = work.clone();
            let sleepers = sleepers.clone();
            let woke_someone = woke_someone.clone();

            thread::spawn(move || {
                work.set(0);

                loom::sync::atomic::fence(SeqCst);

                let mut hint = 0;
                if sleepers.select(&mut hint).is_some() {
                    woke_someone.store(1, SeqCst);
                }
            })
        };

        let worker = {
            let work = work.clone();
            let sleepers = sleepers.clone();
            let went_to_sleep = went_to_sleep.clone();

            thread::spawn(move || {
                sleepers.set(0);

                loom::sync::atomic::fence(SeqCst);

                if work.is_empty() {
                    went_to_sleep.store(1, SeqCst);
                }
            })
        };

        producer.join().unwrap();
        worker.join().unwrap();

        assert!(
            woke_someone.load(SeqCst) == 1 || went_to_sleep.load(SeqCst) == 0,
            "the worker slept and no producer woke it"
        );
    });
}
