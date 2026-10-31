//! The Coordinator-held executable Environment: its registration and its image
//! realization.
//!
//! Control owns authored definitions and policy versions. Coordinator stores a
//! rebuildable projection of each exact registration, used for Session admission
//! and work coordination, and realizes the package image that registration
//! selects. The two stages share one lifecycle and are always consumed together,
//! so they are one contract. Environment *definitions* stay in
//! `awaken-environment-contract`, which does not depend on provisioning.

mod image_realization;
mod registration;

pub use image_realization::*;
pub use registration::*;

mod session_projection;
pub use session_projection::{ResolvedSessionEnvironment, SessionEnvironmentProjectionSource};

mod session_work;
pub use session_work::*;
