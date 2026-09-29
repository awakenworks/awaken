//! The runtime's randomness source for retry jitter.
//!
//! The retry decision itself — classification, attempt budget, backoff and the
//! provider's requested pause — is the pure
//! [`RetryPolicy::decide`](awaken_runtime_contract::resilience::RetryPolicy::decide)
//! in the runtime contract. That decision takes its jitter as an input so a
//! decision table can name it; this module supplies the one production draw.

/// One uniform draw for [`RetryPolicy::decide`](awaken_runtime_contract::resilience::RetryPolicy::decide):
/// thread-local SplitMix64 — enough randomness for jitter without pulling a
/// rand dependency into the runtime core.
pub(crate) fn jitter_draw() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }
    STATE.with(|state| {
        let mut z = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        state.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    // The standard randomized hash state supplies entropy independently of
    // wall time. SplitMix and the pure retry policy remain the sole draw and
    // decision paths; clock rollback cannot repeat a timestamp/PID seed.
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}
