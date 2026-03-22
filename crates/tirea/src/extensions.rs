//! Feature-gated re-exports of extension crates.
//!
//! Each sub-module corresponds to a `tirea-extension-*` crate. The extensions
//! are activated by the `core` feature (default) or individually.

#[cfg(feature = "core")]
pub mod permission {
    pub use tirea_extension_permission::*;
}

#[cfg(feature = "core")]
pub mod observability {
    pub use tirea_extension_observability::*;
}

#[cfg(feature = "core")]
pub mod handoff {
    pub use tirea_extension_handoff::*;
}

#[cfg(feature = "mcp")]
pub mod mcp {
    pub use tirea_extension_mcp::*;
}

#[cfg(feature = "skills")]
pub mod skills {
    pub use tirea_extension_skills::*;
}
