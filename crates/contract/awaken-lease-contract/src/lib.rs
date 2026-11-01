//! The one ownership-fence kernel.
//!
//! [`fence`] holds the successor law every holder-and-epoch lease and every
//! effect fence projected from one obeys: an [`fence::Epoch`] is never zero
//! and only grows, a [`fence::Tenure`] names one holder at one epoch, a
//! [`fence::Lease`] is a tenure with an expiry, and a [`fence::SlotToken`]
//! names the slot an effect is bound to.
//!
//! [`authority`] derives the process-local timing that proves a durable lease
//! is still owned: renewal cadence, bounded request/retry timing and a local
//! proof deadline from the authority-provided TTL.
//!
//! The wall clock, the persisted-expiry law and the monotonic read point live
//! in `awaken-time`, the Runtime SDK's depth-0 clock leaf. The fence kernel
//! depends on it for `Expiry` and `UtcTime`; nothing else, so the SDK can use
//! the clock without linking the fence.
//!
//! # Boundary
//!
//! Context: `agent-runtime`.
//!
//! Published SDK contract (ADR-0086): every context may read it, so its readers are not listed
//! here; `cargo tree -e normal -i awaken-lease-contract` prints them.

pub mod authority;
pub mod fence;
