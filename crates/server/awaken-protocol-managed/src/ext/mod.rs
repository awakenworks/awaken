//! Our extensions: surfaces and vocabulary that are *not* part of the Claude
//! Managed Agents wire but ride the same host and [`ManagedState`] port.
//!
//! Kept apart from `types` (the pure SDK shapes), `project` (the projection), and
//! `router` (the native routes) so the compatible surface stays uncontaminated:
//! - [`live_inbox`] — the queue/reorder/withdraw edit protocol, mounted beside the
//!   session API as its own router.

pub mod live_inbox;

pub use live_inbox::live_inbox_router;
