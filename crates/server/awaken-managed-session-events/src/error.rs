//! Why a committed prefix cannot be projected.

/// A committed prefix that the timeline cannot project without inventing
/// public facts. Every variant fails the whole candidate closed; the caller
/// keeps its previously published projection.
///
/// Consumer: the protocol adapter, which reports it as an internal Runtime
/// fault with the same message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionError {
    /// Committed evidence is malformed or contradicts itself.
    #[error("{0}")]
    Corrupt(String),
}

impl ProjectionError {
    #[must_use]
    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }
}
