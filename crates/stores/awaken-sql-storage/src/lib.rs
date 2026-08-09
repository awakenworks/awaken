//! Canonical synchronous-port to asynchronous-store runtime bridge.
//!
//! A few durable adapters implement pre-existing synchronous repository ports
//! with async database clients. This crate owns the one scheduling policy for
//! that boundary so adapters cannot independently introduce runtime starvation
//! or nested-`block_on` panics.

use std::future::Future;

use tokio::runtime::{Handle, RuntimeFlavor};

/// The only representation permitted at a `u64`/SQL `BIGINT` authority
/// boundary. SQL backends are signed; silently casting either direction can
/// turn overflow into a negative fence or corruption into a huge valid cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredU64(i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoredU64Error {
    #[error("durable authority value {0} is negative")]
    Negative(i64),
    #[error("durable authority value {0} exceeds signed database range")]
    OutOfRange(u64),
    #[error("durable authority arithmetic overflow")]
    ArithmeticOverflow,
}

impl StoredU64 {
    #[must_use]
    pub const fn database_value(self) -> i64 {
        self.0
    }

    #[must_use]
    pub const fn domain_value(self) -> u64 {
        self.0 as u64
    }

    pub fn checked_add(self, increment: u64) -> Result<Self, StoredU64Error> {
        let value = self
            .domain_value()
            .checked_add(increment)
            .ok_or(StoredU64Error::ArithmeticOverflow)?;
        Self::try_from(value)
    }

    pub fn checked_scale_and_offset(
        self,
        scale: u64,
        offset: usize,
    ) -> Result<Self, StoredU64Error> {
        let offset = u64::try_from(offset).map_err(|_| StoredU64Error::ArithmeticOverflow)?;
        let value = self
            .domain_value()
            .checked_mul(scale)
            .and_then(|value| value.checked_add(offset))
            .ok_or(StoredU64Error::ArithmeticOverflow)?;
        Self::try_from(value)
    }
}

impl TryFrom<u64> for StoredU64 {
    type Error = StoredU64Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value)
            .map(Self)
            .map_err(|_| StoredU64Error::OutOfRange(value))
    }
}

impl TryFrom<i64> for StoredU64 {
    type Error = StoredU64Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            Err(StoredU64Error::Negative(value))
        } else {
            Ok(Self(value))
        }
    }
}

/// Drive a store future on the ambient Tokio runtime from a synchronous port.
///
/// `handle` must identify the runtime currently executing the caller. On a
/// multi-thread runtime, `block_in_place` yields the worker before blocking so
/// the database driver's tasks can continue. Calling a synchronous port from a
/// current-thread runtime cannot make progress safely and therefore fails fast
/// instead of deadlocking. Outside Tokio, a short-lived thread enters `handle`.
pub fn block_on_ambient_runtime<T, F, Fut>(handle: &Handle, make: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    let handle = handle.clone();
    match Handle::try_current() {
        Ok(current) if matches!(current.runtime_flavor(), RuntimeFlavor::MultiThread) => {
            tokio::task::block_in_place(move || handle.block_on(make()))
        }
        Ok(_) => panic!(
            "synchronous async-store ports require a multi-thread Tokio runtime; current-thread runtimes would deadlock"
        ),
        Err(_) => std::thread::spawn(move || handle.block_on(make()))
            .join()
            .expect("ambient store runtime bridge thread panicked"),
    }
}

/// Drive a store future on a store-owned runtime from any caller runtime.
///
/// The owned runtime has independent workers and I/O drivers, so a fresh thread
/// may synchronously enter it without occupying or nesting inside the caller's
/// runtime. `make` constructs the future on that thread and need not be `Send`.
pub fn block_on_owned_runtime<T, F, Fut>(handle: &Handle, make: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    let handle = handle.clone();
    std::thread::spawn(move || handle.block_on(make()))
        .join()
        .expect("owned store runtime bridge thread panicked")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use tokio::runtime::Builder;

    use super::*;

    #[test]
    fn signed_database_boundary_is_fail_closed() {
        assert_eq!(
            StoredU64::try_from(-1_i64),
            Err(StoredU64Error::Negative(-1))
        );
        assert_eq!(StoredU64::try_from(0_i64).unwrap().domain_value(), 0);
        assert_eq!(
            StoredU64::try_from(i64::MAX).unwrap().domain_value(),
            i64::MAX as u64
        );
        assert!(matches!(
            StoredU64::try_from(i64::MAX as u64 + 1),
            Err(StoredU64Error::OutOfRange(_))
        ));
    }

    proptest! {
        /// Cause/effect graph: C1 a SQL value is negative or non-negative; C2 a
        /// domain value fits or exceeds `i64::MAX`; C3 arithmetic stays in range
        /// or crosses it. Effects are E1 exact round-trip or E2 a typed error with
        /// no clamping, wrapping, or default substitution.
        ///
        /// | Rule | SQL sign | domain range | arithmetic | Effect |
        /// | P1 | non-negative | fits | n/a | E1 exact round-trip |
        /// | P2 | negative | n/a | n/a | E2 Negative |
        /// | P3 | n/a | exceeds | n/a | E2 OutOfRange |
        /// | P4 | n/a | fits | result fits | E1 exact result |
        /// | P5 | n/a | fits | result exceeds/overflows | E2 typed error |
        #[test]
        fn stored_u64_matches_checked_integer_semantics(
            signed in any::<i64>(),
            domain in any::<u64>(),
            increment in any::<u64>(),
            scale in any::<u64>(),
            offset in any::<usize>(),
        ) {
            match StoredU64::try_from(signed) {
                Ok(stored) => {
                    prop_assert!(signed >= 0, "P1");
                    prop_assert_eq!(stored.database_value(), signed, "P1");
                    prop_assert_eq!(stored.domain_value(), signed as u64, "P1");
                }
                Err(error) => {
                    prop_assert!(signed < 0, "P2");
                    prop_assert_eq!(error, StoredU64Error::Negative(signed), "P2");
                }
            }

            match StoredU64::try_from(domain) {
                Ok(stored) => {
                    prop_assert!(domain <= i64::MAX as u64, "P1/P4/P5");
                    prop_assert_eq!(stored.domain_value(), domain, "P1");
                    let expected_add = domain
                        .checked_add(increment)
                        .filter(|value| *value <= i64::MAX as u64);
                    prop_assert_eq!(
                        stored.checked_add(increment).ok().map(StoredU64::domain_value),
                        expected_add,
                        "P4/P5"
                    );
                    let expected_scaled = u64::try_from(offset)
                        .ok()
                        .and_then(|offset| domain.checked_mul(scale)?.checked_add(offset))
                        .filter(|value| *value <= i64::MAX as u64);
                    prop_assert_eq!(
                        stored.checked_scale_and_offset(scale, offset).ok().map(StoredU64::domain_value),
                        expected_scaled,
                        "P4/P5"
                    );
                }
                Err(error) => {
                    prop_assert!(domain > i64::MAX as u64, "P3");
                    prop_assert_eq!(error, StoredU64Error::OutOfRange(domain), "P3");
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn ambient_bridge_yields_its_only_runtime_worker() {
        // Cause/effect graph: C1 port uses the ambient runtime; C2 call begins
        // inside/outside Tokio; C3 ambient runtime is multi/current-thread.
        // Effects: E1 completes with replacement capacity; E2 enters the live
        // runtime from a helper thread; E3 fails fast instead of deadlocking.
        //
        // | Rule | C1 ambient | C2 inside | C3 flavor       | Effect |
        // |---|---|---|---|---|
        // | D1 | T | T | multi-thread(1) | E1 |
        // | D2 | T | F | multi-thread    | E2 (covered below) |
        // | D3 | T | T | current-thread  | E3 (covered below) |
        let scheduled = tokio::spawn(async { 42_u8 });
        let value = block_on_ambient_runtime(&Handle::current(), move || async move {
            scheduled.await.expect("scheduled store future")
        });
        assert_eq!(value, 42, "D1");
    }

    #[test]
    fn ambient_bridge_enters_a_live_runtime_from_a_plain_thread() {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let value = block_on_ambient_runtime(runtime.handle(), || async { 42_u8 });
        assert_eq!(value, 42, "D2");
    }

    #[test]
    fn ambient_bridge_rejects_a_current_thread_runtime() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(async {
                block_on_ambient_runtime(&Handle::current(), || async { 42_u8 })
            })
        }));
        assert!(result.is_err(), "D3");
    }

    #[test]
    fn owned_bridge_is_independent_of_a_current_thread_caller() {
        // Owned-runtime decision rule: a distinct runtime with an active worker
        // always uses the helper-thread path, regardless of the caller flavor.
        // This preserves E1 completion without treating it as ambient capacity.
        let owned = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("owned runtime");
        let caller = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("caller runtime");
        let target = owned.handle().clone();
        let scheduled = target.spawn(async { 42_u8 });
        let value = caller.block_on(async move {
            block_on_owned_runtime(&target, move || async move {
                scheduled.await.expect("owned store future")
            })
        });
        assert_eq!(value, 42);
    }
}
