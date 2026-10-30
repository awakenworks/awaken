// @formal surface obligation=session_run.reservation_precedes_activity
// @formal reason: Typed admission disposition carries the existing owner's receipt decision;
// @formal reason: it grants no execution authority and does not prove the durable receipt itself.
//! Programmatic diagnostics for SDK admission and infrastructure failures.

use std::borrow::Cow;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Diagnostic(#[from] RuntimeDiagnostic),
    #[error("runtime resolution failed: {0}")]
    Resolution(String),
    #[error("runtime execution failed: {0}")]
    Execution(String),
    #[error("runtime commit failed: {0}")]
    Commit(String),
    /// Internal signal consumed by the runtime loop and converted into the
    /// terminal `Failure::StateConflict`; it must not escape `RunExecutor`.
    #[error("runtime state batch conflicts")]
    StateConflict,
    /// Internal signal consumed by the runtime loop as `EndCause::Indeterminate`.
    /// A logical terminal outcome is not evidence that an external effect stopped.
    #[error("runtime external execution indeterminate")]
    Indeterminate,
}

impl Error {
    /// Legacy string variants have conservative fallback classifications.
    pub fn kind(&self) -> RuntimeErrorKind {
        match self {
            Self::Diagnostic(error) => error.kind(),
            Self::Resolution(_) => RuntimeErrorKind::InvalidRequest,
            Self::StateConflict => RuntimeErrorKind::Conflict,
            Self::Execution(_) | Self::Commit(_) | Self::Indeterminate => {
                RuntimeErrorKind::Internal
            }
        }
    }

    /// A stable code does not imply that an invocation is safe to retry.
    pub fn code(&self) -> &str {
        match self {
            Self::Diagnostic(error) => error.code(),
            Self::Resolution(_) => "resolution_failed",
            Self::Execution(_) => "execution_failed",
            Self::Commit(_) => "commit_failed",
            Self::StateConflict => "state_conflict",
            Self::Indeterminate => "execution_indeterminate",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<awaken_agent_contract::thread::commit::coordinator::Error> for Error {
    fn from(source: awaken_agent_contract::thread::commit::coordinator::Error) -> Self {
        RuntimeDiagnostic::new(RuntimeErrorKind::Internal, "commit_failed", source).into()
    }
}

/// Transient admission evidence, never a persisted execution result. A closed
/// decision requires the admitting owner's durable refusal; unavailable reads,
/// lease loss and undecided admission are deferred instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAdmissionDisposition {
    Deferred,
    Closed,
}

#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct RunAdmissionFailure {
    disposition: RunAdmissionDisposition,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl RunAdmissionFailure {
    pub fn new(
        disposition: RunAdmissionDisposition,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            disposition,
            source: Box::new(source),
        }
    }

    pub fn disposition(&self) -> RunAdmissionDisposition {
        self.disposition
    }
}

/// Inspect a typed original cause before an adapter projects its display text.
pub fn run_admission_disposition(error: &Error) -> Option<RunAdmissionDisposition> {
    use std::error::Error as _;
    let Error::Diagnostic(diagnostic) = error else {
        return None;
    };
    diagnostic
        .source()?
        .downcast_ref::<RunAdmissionFailure>()
        .map(RunAdmissionFailure::disposition)
}

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
    code: Cow<'static, str>,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl RuntimeDiagnostic {
    pub fn new(
        kind: RuntimeErrorKind,
        code: impl Into<Cow<'static, str>>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            code: code.into(),
            source: Box::new(source),
        }
    }

    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}
