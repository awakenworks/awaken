use async_trait::async_trait;
use bytes::Bytes;

use crate::error::SandboxError;

/// Seam between the sandbox provider and a runtime credential store.
///
/// Implementations bridge to the runtime's credential broker (or any other
/// secret store) without importing `awaken-runtime` from this crate.  The
/// only values that cross the seam are the opaque `reference` string and the
/// raw secret bytes — never structured credential objects.
#[async_trait]
pub trait SecretBroker: Send + Sync {
    /// Resolve a broker reference (e.g. `"provider://id/key"`) to raw bytes.
    ///
    /// The returned bytes are written to the sandbox file at the mount's
    /// target path.  The provider caches nothing — every call to
    /// `realize_mount` for a `MountSource::Secret` triggers one `resolve`.
    async fn resolve(&self, reference: &str) -> Result<Bytes, SandboxError>;

    /// Write back (possibly refreshed) bytes to the broker after a run.
    ///
    /// Called automatically by [`LocalSandboxProvider::collect_writebacks`]
    /// for every `MountSource::Secret` mount that has `ReadWrite` access and
    /// `Durable` lifetime.  No-op when the file does not exist in the sandbox
    /// (the agent never touched it).
    async fn write_back(&self, reference: &str, bytes: Bytes) -> Result<(), SandboxError>;
}
