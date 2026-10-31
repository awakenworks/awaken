// @formal surface exclude=sync_primitive
// @formal reason: Wraps the std locks under the one poisoning law and decides nothing else; its exhaustive decision table is src/tests.rs.
//! The workspace's one lock-poisoning law.
//!
//! A `std::sync` lock is poisoned when a holder panics while holding it. What
//! the poison means depends on what the lock protects, so the answer is chosen
//! once, by the type of the lock, and never again at a call site:
//!
//! * [`InvariantLock`] and [`InvariantRwLock`] guard a critical section that
//!   keeps an invariant across fields or entries. A holder that panicked may
//!   have left it half-written, so every later acquisition is refused with
//!   [`LockPoisoned`], and the caller propagates that refusal as its own typed
//!   error.
//! * [`LeafLock`] and [`LeafRwLock`] guard state that is valid after any panic:
//!   a cache, a counter, a registry of independent entries, a recorded-call
//!   log. Every critical section is one mutation of the guarded value, so the
//!   poison carries no information; acquisition recovers the guard and never
//!   fails.
//!
//! Production code does not call `std::sync::Mutex::lock` or
//! `std::sync::RwLock::{read, write}` itself: the workspace `clippy.toml` denies
//! them, and this crate is the one place that reads a `std::sync` lock. The
//! design note is `docs/design/lock-poisoning.md`.
//!
//! This crate belongs to the Runtime SDK and is a depth-0 contract leaf: it
//! depends on no crate.

#![warn(unreachable_pub)]
// The one place the poisoning law reads the `std::sync` locks it wraps.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::sync::{
    Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError,
};

/// An invariant lock whose holder panicked. The state it guards may be
/// half-written, so the operation that needed it is refused.
///
/// It names the guarded type for diagnostics; the owning crate decides the
/// error class once, in its `From<LockPoisoned>` conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LockPoisoned {
    guarded: &'static str,
}

impl LockPoisoned {
    fn of<T: ?Sized>() -> Self {
        Self {
            guarded: std::any::type_name::<T>(),
        }
    }

    /// The type name of the state the poisoned lock guards.
    pub const fn guarded(&self) -> &'static str {
        self.guarded
    }
}

impl fmt::Display for LockPoisoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lock guarding `{}` is poisoned: a holder panicked mid-update",
            self.guarded
        )
    }
}

impl std::error::Error for LockPoisoned {}

/// Why [`InvariantLock::try_lock`] did not return a guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryLockFailure {
    /// Another holder has the lock now.
    WouldBlock,
    /// A holder panicked; the guarded state is refused.
    Poisoned(LockPoisoned),
}

impl TryLockFailure {
    fn of<T: ?Sized, G>(error: TryLockError<G>) -> Self {
        match error {
            TryLockError::WouldBlock => Self::WouldBlock,
            TryLockError::Poisoned(_) => Self::Poisoned(LockPoisoned::of::<T>()),
        }
    }
}

/// A leaf lock's non-blocking acquisition: poison is recovered, only
/// contention yields `None`.
fn recovered<G>(attempt: Result<G, TryLockError<G>>) -> Option<G> {
    match attempt {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// A mutex whose critical sections keep an invariant: poisoning is refused.
pub struct InvariantLock<T: ?Sized> {
    inner: Mutex<T>,
}

impl<T> InvariantLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    pub fn into_inner(self) -> Result<T, LockPoisoned> {
        self.inner.into_inner().map_err(|_| LockPoisoned::of::<T>())
    }
}

impl<T: ?Sized> InvariantLock<T> {
    pub fn lock(&self) -> Result<MutexGuard<'_, T>, LockPoisoned> {
        self.inner.lock().map_err(|_| LockPoisoned::of::<T>())
    }

    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockFailure> {
        self.inner.try_lock().map_err(TryLockFailure::of::<T, _>)
    }

    pub fn get_mut(&mut self) -> Result<&mut T, LockPoisoned> {
        self.inner.get_mut().map_err(|_| LockPoisoned::of::<T>())
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}

/// A reader-writer lock whose critical sections keep an invariant: poisoning
/// is refused.
pub struct InvariantRwLock<T: ?Sized> {
    inner: RwLock<T>,
}

impl<T> InvariantRwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: RwLock::new(value),
        }
    }

    pub fn into_inner(self) -> Result<T, LockPoisoned> {
        self.inner.into_inner().map_err(|_| LockPoisoned::of::<T>())
    }
}

impl<T: ?Sized> InvariantRwLock<T> {
    pub fn read(&self) -> Result<RwLockReadGuard<'_, T>, LockPoisoned> {
        self.inner.read().map_err(|_| LockPoisoned::of::<T>())
    }

    pub fn write(&self) -> Result<RwLockWriteGuard<'_, T>, LockPoisoned> {
        self.inner.write().map_err(|_| LockPoisoned::of::<T>())
    }

    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockFailure> {
        self.inner.try_read().map_err(TryLockFailure::of::<T, _>)
    }

    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockFailure> {
        self.inner.try_write().map_err(TryLockFailure::of::<T, _>)
    }

    pub fn get_mut(&mut self) -> Result<&mut T, LockPoisoned> {
        self.inner.get_mut().map_err(|_| LockPoisoned::of::<T>())
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}

/// A mutex over state that stays valid after any panic: poisoning is not
/// observable.
pub struct LeafLock<T: ?Sized> {
    inner: Mutex<T>,
}

impl<T> LeafLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.inner
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> LeafLock<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The guard, unless another holder has the lock now.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        recovered(self.inner.try_lock())
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A reader-writer lock over state that stays valid after any panic:
/// poisoning is not observable.
pub struct LeafRwLock<T: ?Sized> {
    inner: RwLock<T>,
}

impl<T> LeafRwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: RwLock::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.inner
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> LeafRwLock<T> {
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The read guard, unless a writer has the lock now.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        recovered(self.inner.try_read())
    }

    /// The write guard, unless another holder has the lock now.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        recovered(self.inner.try_write())
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

macro_rules! lock_conveniences {
    ($($lock:ident),+ $(,)?) => {$(
        impl<T: Default> Default for $lock<T> {
            fn default() -> Self {
                Self::new(T::default())
            }
        }

        impl<T> From<T> for $lock<T> {
            fn from(value: T) -> Self {
                Self::new(value)
            }
        }

        impl<T: ?Sized + fmt::Debug> fmt::Debug for $lock<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.inner, f)
            }
        }
    )+};
}

lock_conveniences!(InvariantLock, InvariantRwLock, LeafLock, LeafRwLock);

#[cfg(test)]
mod tests;
