//! The workspace's one wall clock and its one persisted-expiry law.
//!
//! Every component that stamps a durable record, mints a deadline or judges
//! whether one has passed takes its time from this crate:
//!
//! * [`WallClock`] is the only wall-clock port. An owner receives an
//!   `Arc<dyn WallClock>` at construction, a test hands it a manual clock from
//!   `awaken-lease-testkit`, and the composition root passes [`system_clock()`]
//!   once. The system implementation is private, so the real clock cannot be
//!   read from a concrete type below the root; [`WallClockExt`] adds the
//!   millisecond and RFC 3339 spellings to every clock.
//! * [`UtcTime`] is one reading. It converts to and from epoch milliseconds and
//!   Unix nanoseconds and renders and parses through the one RFC 3339 codec in
//!   [`utc`].
//! * [`Expiry`] is a persisted deadline and carries the single boundary law:
//!   live while `now < expiry`, expired from `now >= expiry` on.
//!   [`NotBefore`] is its dual: due from `now >= not_before` on.
//! * [`mono`] is the single monotonic read point for timeouts and elapsed
//!   durations. It reads tokio's clock, so a paused test runtime controls it.
//!
//! **Pre-epoch readings saturate to the Unix epoch.** That is the one strategy
//! for time before 1970: a host clock set earlier reads as
//! [`UtcTime::UNIX_EPOCH`], and a negative persisted millisecond decodes to the
//! epoch (so a negative persisted expiry is already expired).
//!
//! This crate belongs to the Runtime SDK and is a depth-0 contract leaf: it
//! depends on no awaken crate. The Platform's ownership-fence kernel,
//! `awaken-lease-contract`, depends on it for `Expiry` and `UtcTime`.

mod clock;
mod expiry;
pub mod mono;
mod time;
pub mod utc;

pub use clock::{WallClock, WallClockExt, system_clock};
pub use expiry::{Expiry, NotBefore};
pub use time::UtcTime;
