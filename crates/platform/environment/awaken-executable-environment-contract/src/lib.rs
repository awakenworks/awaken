//! The Coordinator-held executable Environment: its registration and its image
//! realization.
//!
//! Control owns authored definitions and policy versions. Coordinator stores a
//! rebuildable projection of each exact registration, used for Session admission
//! and work coordination, and realizes the package image that registration
//! selects. The two stages share one lifecycle and are always consumed together,
//! so they are one contract. Environment *definitions* stay in
//! `awaken-environment-contract`, which does not depend on provisioning.
//!
//! # Boundary
//!
//! Context: `environment`.
//!
//! Implemented by: `awaken-control`, `awaken-coordinator`, `awaken-environment-application`,
//! `awaken-environment-image-build`, `awaken-environment-package-image-builder`,
//! `awaken-executable-environment-catalog`, `awaken-session-application`, `awaken-work-store`.
//!
//! Called by: `awaken-control`, `awaken-coordinator`, `awaken-environment-application`,
//! `awaken-environment-image-build`, `awaken-environment-package-image-builder`,
//! `awaken-executable-environment-catalog`, `awaken-process-startup`, `awaken-protocol-managed`,
//! `awaken-protocol-managed-testkit`, `awaken-session-application`, `awaken-store-conformance`,
//! `awaken-work-store`.

mod image_build_demand;
mod image_realization;
mod registration;

pub use image_build_demand::EnvironmentImageBuildDemand;
pub use image_realization::*;
pub use registration::*;

mod session_projection;
pub use session_projection::{ResolvedSessionEnvironment, SessionEnvironmentProjectionSource};

mod session_work;
pub use session_work::*;
