//! Local and namespace sandbox providers backed by [`awaken_file_store::FileStore`].
//!
//! Both providers are async throughout; no blocking I/O occurs on the async executor.

mod error;
mod mount;
mod provider;
mod sandbox;
mod source;
#[cfg(test)]
mod tests;

pub use error::SandboxError;
pub use mount::Mount;
pub use provider::{LocalSandboxProvider, NamespaceSandboxProvider, SandboxProvider};
pub use sandbox::Sandbox;
pub use source::Source;
