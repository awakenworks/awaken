//! The one local elapsed clock behind relative authority proofs.
//!
//! A Worker or Runtime Host proves "this lease still has N remaining" by
//! pairing a scheduling instant with an elapsed-clock sample. The scoped
//! process adapter later converts the same remaining authority into a physical
//! OS cutoff. Both sides must read the *same* clock, or a suspend could let the
//! proof outlive the cutoff. On Linux that clock is the suspend-inclusive
//! `CLOCK_BOOTTIME`.
//!
//! This is an OS clock reading, deliberately exposed as plain functions rather
//! than an injectable port: substituting it would break the equality with the
//! physical cutoff that is its whole purpose. It is unrelated to the injected
//! wall clock that stamps observation times, and its values are meaningful only
//! during the current boot — never durable authority.

use awaken_lease_contract::authority::AuthorityProofStarted;

/// Sample the suspend-inclusive BOOTTIME clock shared by the authority proof
/// and the scoped process physical cutoff.
///
/// Named for the OS clock it reads (it was `scoped::execution_elapsed_time`
/// while only the scoped launcher owned it); values are meaningful only during
/// this boot, never durable authority.
#[cfg(target_os = "linux")]
pub fn boottime_elapsed() -> std::io::Result<std::time::Duration> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid BOOTTIME clock sample",
        )
    };
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let seconds = u64::try_from(time.tv_sec).map_err(|_| invalid())?;
    let nanos = u32::try_from(time.tv_nsec).map_err(|_| invalid())?;
    if nanos >= 1_000_000_000 {
        return Err(invalid());
    }
    Ok(std::time::Duration::new(seconds, nanos))
}

/// Sample the one local authority clock independently of timer scheduling.
/// Linux reuses the physical cutoff's suspend-inclusive BOOTTIME. Other
/// platforms preserve their existing Instant-based proof behavior; this is
/// not a claim of suspend-inclusive or process-tree qualification there.
///
/// The scheduling half is the one monotonic scheduling point
/// (`awaken_time::mono`); only the authority half reads this crate's
/// clock, so scheduling never stands in for the physical cutoff's clock.
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        clippy::disallowed_methods,
        reason = "without BOOTTIME the authority half samples the host monotonic clock directly"
    )
)]
pub fn authority_proof_started() -> std::io::Result<AuthorityProofStarted> {
    let scheduled = awaken_time::mono::now();
    #[cfg(target_os = "linux")]
    let elapsed = boottime_elapsed()?;
    #[cfg(not(target_os = "linux"))]
    let elapsed = {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let origin = *ORIGIN.get_or_init(std::time::Instant::now);
        std::time::Instant::now()
            .checked_duration_since(origin)
            .ok_or_else(|| std::io::Error::other("local authority elapsed clock regressed"))?
    };
    Ok(AuthorityProofStarted::from_clock_sample(scheduled, elapsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_observation_uses_the_platform_clock_without_a_second_origin_on_linux() {
        // A1 consecutive captures -> nonnegative elapsed and ordered scheduling.
        // A2 Linux captures lie between samples of the exact clock used by
        // scoped cutoff, not a new process-relative or wall-clock epoch.
        #[cfg(target_os = "linux")]
        let before = boottime_elapsed().unwrap();
        let first = authority_proof_started().unwrap();
        let second = authority_proof_started().unwrap();
        assert!(first.elapsed_at(second).is_some());
        assert!(second.scheduled() >= first.scheduled());
        #[cfg(target_os = "linux")]
        {
            let after = boottime_elapsed().unwrap();
            assert!(before <= first.elapsed_clock());
            assert!(second.elapsed_clock() <= after);
        }
    }
}
