//! The one ownership-fence kernel.
//!
//! [`fence`] holds the successor law every holder-and-epoch lease and every
//! effect fence projected from one obeys: an [`fence::Epoch`] is never zero
//! and only grows, a [`fence::Tenure`] names one holder at one epoch, a
//! [`fence::Lease`] is a tenure with an expiry, and a [`fence::SlotToken`]
//! names the slot an effect is bound to.
//!
//! The wall clock, the persisted-expiry law and the monotonic read point live
//! in `awaken-time`, the Runtime SDK's depth-0 clock leaf. The fence kernel
//! depends on it for `Expiry` and `UtcTime`; nothing else, so the SDK can use
//! the clock without linking the fence.

pub mod fence;
