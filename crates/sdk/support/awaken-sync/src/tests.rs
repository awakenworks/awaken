//! Decision table of the lock-poisoning law.
//!
//! | Row | Lock            | Cause                              | Effect                                             |
//! |-----|-----------------|------------------------------------|----------------------------------------------------|
//! | I1  | invariant mutex | healthy                            | `lock`/`get_mut`/`into_inner` return the value     |
//! | I2  | invariant mutex | a holder panicked mid-update       | every access refuses with `LockPoisoned`           |
//! | I3  | invariant mutex | held by another guard              | `try_lock` is `WouldBlock`, not `Poisoned`         |
//! | I4  | invariant rw    | healthy                            | `read`/`write` return the value                    |
//! | I5  | invariant rw    | a writer panicked mid-update       | `read`/`write`/`into_inner` refuse                 |
//! | I6  | invariant rw    | a reader panicked                  | not poisoned: a read cannot half-write             |
//! | I7  | invariant rw    | write-held / poisoned              | `try_read`/`try_write` are `WouldBlock` / `Poisoned` |
//! | L1  | leaf mutex      | healthy                            | `lock` returns the guard                           |
//! | L2  | leaf mutex      | a holder panicked mid-update       | the value as the holder left it is recovered       |
//! | L3  | leaf mutex      | held by another guard              | `try_lock` is `None`                               |
//! | L4  | leaf rw         | healthy                            | `read`/`write` return the value                    |
//! | L5  | leaf rw         | a writer panicked mid-update       | the value as the writer left it is recovered       |
//! | L6  | leaf rw         | write-held / poisoned              | `try_*` is `None` / recovers the guard             |
//! | E1  | refusal         | any poisoned invariant lock        | names the guarded type; is not `Debug`, so not unwrappable |
//! | E2  | refusal         | two locks over the same type       | equal refusals; the crate maps the class once      |

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

/// The guard of a healthy invariant lock. `LockPoisoned` is not `Debug`, so the
/// test panics with the refusal's own text instead of unwrapping it.
fn healthy<G>(acquired: Result<G, LockPoisoned>) -> G {
    acquired.unwrap_or_else(|poisoned| panic!("{poisoned}"))
}

/// The guard of an uncontended, healthy invariant lock.
fn free<G>(acquired: Result<G, TryLockFailure>) -> G {
    acquired.unwrap_or_else(|_| panic!("the invariant lock is held or poisoned"))
}

/// Poison by the only route production has: a holder panics while holding.
fn panic_while_holding(hold: impl FnOnce()) {
    let unwound = catch_unwind(AssertUnwindSafe(hold));
    assert!(unwound.is_err(), "the holder must panic to poison the lock");
}

#[test]
fn i1_healthy_invariant_lock_returns_the_value() {
    let mut lock = InvariantLock::new(vec![1]);
    healthy(lock.lock()).push(2);
    assert!(!lock.is_poisoned());
    healthy(lock.get_mut()).push(3);
    assert_eq!(free(lock.try_lock()).as_slice(), &[1, 2, 3]);
    assert!(lock.into_inner() == Ok(vec![1, 2, 3]));
}

#[test]
fn i2_poisoned_invariant_lock_refuses_every_access() {
    let mut lock = InvariantLock::new(vec![1]);
    panic_while_holding(|| {
        let mut held = healthy(lock.lock());
        held.push(2);
        panic!("holder panics between two halves of an update");
    });
    let refusal = LockPoisoned::of::<Vec<i32>>();
    assert!(lock.is_poisoned());
    assert!(lock.lock().err() == Some(refusal));
    assert!(lock.try_lock().err() == Some(TryLockFailure::Poisoned(refusal)));
    assert!(lock.get_mut().err() == Some(refusal));
    assert!(lock.into_inner() == Err(refusal));
}

#[test]
fn i3_held_invariant_lock_would_block_without_poison() {
    let lock = InvariantLock::new(0_u8);
    let _held = healthy(lock.lock());
    assert!(lock.try_lock().err() == Some(TryLockFailure::WouldBlock));
}

#[test]
fn i4_healthy_invariant_rw_lock_returns_the_value() {
    let mut lock = InvariantRwLock::new(1_u32);
    *healthy(lock.write()) += 1;
    assert_eq!(*healthy(lock.read()), 2);
    *healthy(lock.get_mut()) += 1;
    assert!(!lock.is_poisoned());
    assert!(lock.into_inner() == Ok(3));
}

#[test]
fn i5_writer_panic_poisons_the_invariant_rw_lock() {
    let mut lock = InvariantRwLock::new(String::from("a"));
    panic_while_holding(|| {
        healthy(lock.write()).push('b');
        let _held = healthy(lock.write());
        panic!("writer panics mid-update");
    });
    let refusal = LockPoisoned::of::<String>();
    assert!(lock.is_poisoned());
    assert!(lock.read().err() == Some(refusal));
    assert!(lock.write().err() == Some(refusal));
    assert!(lock.get_mut().err() == Some(refusal));
    assert!(lock.into_inner() == Err(refusal));
}

#[test]
fn i6_reader_panic_leaves_the_invariant_rw_lock_healthy() {
    let lock = InvariantRwLock::new(5_u8);
    panic_while_holding(|| {
        let _read = healthy(lock.read());
        panic!("reader panics");
    });
    assert!(!lock.is_poisoned());
    assert_eq!(*healthy(lock.read()), 5);
}

#[test]
fn l1_healthy_leaf_lock_returns_the_guard() {
    let mut lock = LeafLock::new(vec![1]);
    lock.lock().push(2);
    lock.get_mut().push(3);
    assert_eq!(lock.try_lock().expect("free").as_slice(), &[1, 2, 3]);
    assert_eq!(lock.into_inner(), vec![1, 2, 3]);
}

#[test]
fn l2_poisoned_leaf_lock_recovers_the_value_the_holder_left() {
    let mut lock = LeafLock::new(vec![1]);
    panic_while_holding(|| {
        lock.lock().push(2);
        let _held = lock.lock();
        panic!("holder panics after a whole mutation");
    });
    assert_eq!(lock.lock().as_slice(), &[1, 2]);
    lock.lock().push(3);
    assert_eq!(lock.try_lock().expect("free").as_slice(), &[1, 2, 3]);
    lock.get_mut().push(4);
    assert_eq!(lock.into_inner(), vec![1, 2, 3, 4]);
}

#[test]
fn l3_held_leaf_lock_try_lock_is_none() {
    let lock = LeafLock::new(0_u8);
    let _held = lock.lock();
    assert!(lock.try_lock().is_none());
}

#[test]
fn l4_healthy_leaf_rw_lock_returns_the_value() {
    let mut lock = LeafRwLock::new(1_u32);
    *lock.write() += 1;
    assert_eq!(*lock.read(), 2);
    *lock.get_mut() += 1;
    assert_eq!(lock.into_inner(), 3);
}

#[test]
fn l5_poisoned_leaf_rw_lock_recovers_the_value_the_writer_left() {
    let mut lock = LeafRwLock::new(String::from("a"));
    panic_while_holding(|| {
        lock.write().push('b');
        let _held = lock.write();
        panic!("writer panics after a whole mutation");
    });
    assert_eq!(lock.read().as_str(), "ab");
    lock.write().push('c');
    lock.get_mut().push('d');
    assert_eq!(lock.into_inner(), "abcd");
}

#[test]
fn e1_refusal_names_the_guarded_state() {
    let lock = InvariantLock::new(std::collections::BTreeMap::<String, u8>::new());
    panic_while_holding(|| {
        let _held = healthy(lock.lock());
        panic!("poison");
    });
    let refusal = lock.lock().expect_err("poisoned");
    assert!(refusal.guarded().contains("BTreeMap"));
    let message = refusal.to_string();
    assert!(
        message.contains("BTreeMap") && message.contains("poisoned"),
        "{message}"
    );
}

#[test]
fn e2_refusals_over_the_same_state_are_equal() {
    let first = InvariantLock::new(1_u64);
    let second = InvariantRwLock::new(2_u64);
    panic_while_holding(|| {
        let _held = healthy(first.lock());
        panic!("poison");
    });
    panic_while_holding(|| {
        let _held = healthy(second.write());
        panic!("poison");
    });
    assert!(first.lock().err() == second.read().err());
}

#[test]
fn conveniences_construct_like_the_std_locks() {
    let invariant: InvariantLock<Vec<u8>> = InvariantLock::default();
    let leaf: LeafRwLock<u8> = 7.into();
    assert!(healthy(invariant.lock()).is_empty());
    assert_eq!(*leaf.read(), 7);
    assert!(format!("{invariant:?}").contains("Mutex"));
    assert!(format!("{leaf:?}").contains("RwLock"));
}

#[test]
fn i7_invariant_rw_try_acquisition_tells_contention_from_poison() {
    let lock = InvariantRwLock::new(1_u8);
    {
        let _writer = healthy(lock.write());
        assert!(lock.try_read().err() == Some(TryLockFailure::WouldBlock));
        assert!(lock.try_write().err() == Some(TryLockFailure::WouldBlock));
    }
    panic_while_holding(|| {
        let _writer = healthy(lock.write());
        panic!("writer panics mid-update");
    });
    let poisoned = TryLockFailure::Poisoned(LockPoisoned::of::<u8>());
    assert!(lock.try_read().err() == Some(poisoned));
    assert!(lock.try_write().err() == Some(poisoned));
}

#[test]
fn l6_leaf_rw_try_acquisition_yields_only_to_contention() {
    let lock = LeafRwLock::new(1_u8);
    {
        let _writer = lock.write();
        assert!(lock.try_read().is_none());
        assert!(lock.try_write().is_none());
    }
    panic_while_holding(|| {
        *lock.write() = 2;
        let _writer = lock.write();
        panic!("writer panics after a whole mutation");
    });
    assert_eq!(*lock.try_read().expect("free"), 2);
    *lock.try_write().expect("free") = 3;
    assert_eq!(*lock.read(), 3);
}
