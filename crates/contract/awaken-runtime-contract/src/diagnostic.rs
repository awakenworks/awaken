//! Programmatic diagnostics for SDK admission and infrastructure failures.

/// Classification describes the failure, never permission to replay an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeErrorKind {
    InvalidConfiguration,
    InvalidRequest,
    Conflict,
    Unavailable,
    Internal,
}

/// Retains the original cause while exposing a stable machine-readable code.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct RuntimeDiagnostic {
    kind: RuntimeErrorKind,
    code: &'static str,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl RuntimeDiagnostic {
    pub fn new(
        kind: RuntimeErrorKind,
        code: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            code,
            source: Box::new(source),
        }
    }

    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}
