//! Container runtime failure classes and their mapping onto the Sandbox port.

use awaken_provisioning_contract as pc;

/// A container/pod runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("container {0:?} not found")]
    NotFound(String),
    #[error("container runtime failed: {0}")]
    Backend(String),
    /// The backend API rejected the request because the addressed object's
    /// current state contradicts it (HTTP 409).
    #[error("container runtime conflict: {0}")]
    Conflict(String),
    /// The backend API refused the caller's credentials or authority
    /// (HTTP 401/403).
    #[error("container runtime refused authority: {0}")]
    Unauthorized(String),
    /// The backend has no room for this request now: a Kubernetes namespace
    /// quota refused the Create (which therefore had no API effect), or the
    /// scheduler could not place the Pod within the configured bound
    /// (ADR-0092). Another Worker, a raised quota or a new node may succeed.
    #[error("container runtime has no capacity: {0}")]
    CapacityUnavailable(String),
    /// The adapter crossed a backend mutation boundary and could not prove
    /// whether that exact effect converged. Callers must retain every staged
    /// participant that the physical object may still reference; treating this
    /// as an ordinary backend rejection would turn response loss into dangling
    /// host binds or prematurely torn-down Memory mounts.
    #[error("container runtime effect may have committed: {0}")]
    MayHaveCommitted(String),
    /// The current invocation reached another idempotent provider boundary
    /// after its immutable effect generation was renewed. The caller may retry
    /// only that same effect with a monotonic fence supplied by the existing
    /// Session authority; it must retain all staged participants meanwhile.
    #[error("container runtime effect fence expired before the next provider boundary")]
    EffectFenceExpired,
}

impl From<RuntimeError> for pc::SandboxError {
    /// The runtime adapter already distinguishes absence, a stale fence, and an
    /// effect whose outcome is unknown; keep those classes at the port.
    fn from(error: RuntimeError) -> Self {
        let detail = error.to_string();
        match error {
            RuntimeError::NotFound(_) => Self::NotFound(detail),
            RuntimeError::EffectFenceExpired => Self::FenceRejected(detail),
            RuntimeError::MayHaveCommitted(_) => Self::Transport(detail),
            RuntimeError::Conflict(_) => Self::Conflict(detail),
            RuntimeError::Unauthorized(_) => Self::Unauthorized(detail),
            RuntimeError::CapacityUnavailable(_) => Self::CapacityUnavailable(detail),
            RuntimeError::Backend(cause) => Self::backend("container runtime", cause),
        }
    }
}

impl RuntimeError {
    /// Classify a backend API response by its HTTP status. Only the statuses
    /// callers act on get their own class; every other response stays a
    /// mechanism failure.
    pub fn from_api_status(status: u16, detail: String) -> Self {
        match status {
            404 => Self::NotFound(detail),
            409 => Self::Conflict(detail),
            401 | 403 => Self::Unauthorized(detail),
            _ => Self::Backend(detail),
        }
    }

    /// Classify an error observed after this invocation crossed its first
    /// backend mutation boundary. The wrapper is idempotent so nested runtime
    /// adapters cannot erase the stronger outcome. A capacity refusal keeps its
    /// class: the refused request itself had no effect, and every participant
    /// an earlier step created belongs to the same exact effect identity, which
    /// the next attempt recovers or the existing disposal removes.
    pub fn after_mutation(self) -> Self {
        match self {
            Self::MayHaveCommitted(_) | Self::EffectFenceExpired | Self::CapacityUnavailable(_) => {
                self
            }
            error => Self::MayHaveCommitted(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::resilience::{Classify, Disposition};

    #[test]
    fn shared_api_status_projection_preserves_class_and_detail() {
        // E1/E3 decision table: only an extracted backend API status is an
        // input to classification. 404 => absence, 409 => conflict,
        // 401/403 => authority refusal, all other status values => Backend.
        // Detail is opaque: quota/status-looking text cannot add an authority,
        // absence or capacity fact. The exact existing port projection retains
        // both detail and disposition; no transport fake/classifier is used.
        let rows = [
            (0, "Backend"),
            (200, "Backend"),
            (400, "Backend"),
            (401, "Unauthorized"),
            (403, "Unauthorized"),
            (404, "NotFound"),
            (409, "Conflict"),
            (422, "Backend"),
            (429, "Backend"),
            (500, "Backend"),
            (503, "Backend"),
            (u16::MAX, "Backend"),
        ];
        for (status, class) in rows {
            for detail in ["", "opaque detail", "HTTP 404: exceeded quota; Forbidden"] {
                let error = RuntimeError::from_api_status(status, detail.into());
                let (expected, display, disposition) = match class {
                    "NotFound" => {
                        let display = format!("container {detail:?} not found");
                        (
                            pc::SandboxError::NotFound(display.clone()),
                            display,
                            Disposition::Permanent,
                        )
                    }
                    "Conflict" => {
                        let display = format!("container runtime conflict: {detail}");
                        (
                            pc::SandboxError::Conflict(display.clone()),
                            display,
                            Disposition::Permanent,
                        )
                    }
                    "Unauthorized" => {
                        let display = format!("container runtime refused authority: {detail}");
                        (
                            pc::SandboxError::Unauthorized(display.clone()),
                            display,
                            Disposition::Permanent,
                        )
                    }
                    "Backend" => (
                        pc::SandboxError::backend("container runtime", detail),
                        format!("container runtime failed: {detail}"),
                        Disposition::Transient,
                    ),
                    _ => unreachable!("closed decision table"),
                };
                assert_eq!(error.to_string(), display, "status {status}");
                let projected = pc::SandboxError::from(error);
                assert_eq!(projected, expected, "status {status}");
                assert_eq!(projected.disposition(), disposition, "status {status}");
            }
        }
    }

    #[test]
    fn post_mutation_projection_is_total_and_idempotent() {
        // E4 cause/effect table: all seven original classes × zero/one/two
        // crossed-mutation wrappers. Before mutation the port class and exact
        // display are preserved. Ordinary errors strengthen to uncertain
        // Transport exactly once; capacity (known refused request), expired
        // fence (same-effect renewal), and existing uncertainty keep their
        // identity and disposition. Repeated wrapping never adds a second
        // prefix or erases an exception. This is the original method, not a
        // test-only adapter, cleanup authority or alternate retry decision.
        type Row = (
            fn() -> RuntimeError,
            &'static str,
            pc::SandboxError,
            Disposition,
            bool,
        );
        let rows: [Row; 7] = [
            (
                || RuntimeError::NotFound("detail".into()),
                "container \"detail\" not found",
                pc::SandboxError::NotFound("container \"detail\" not found".into()),
                Disposition::Permanent,
                false,
            ),
            (
                || RuntimeError::Backend("detail".into()),
                "container runtime failed: detail",
                pc::SandboxError::backend("container runtime", "detail"),
                Disposition::Transient,
                false,
            ),
            (
                || RuntimeError::Conflict("detail".into()),
                "container runtime conflict: detail",
                pc::SandboxError::Conflict("container runtime conflict: detail".into()),
                Disposition::Permanent,
                false,
            ),
            (
                || RuntimeError::Unauthorized("detail".into()),
                "container runtime refused authority: detail",
                pc::SandboxError::Unauthorized(
                    "container runtime refused authority: detail".into(),
                ),
                Disposition::Permanent,
                false,
            ),
            (
                || RuntimeError::CapacityUnavailable("detail".into()),
                "container runtime has no capacity: detail",
                pc::SandboxError::CapacityUnavailable(
                    "container runtime has no capacity: detail".into(),
                ),
                Disposition::Transient,
                true,
            ),
            (
                || RuntimeError::MayHaveCommitted("detail".into()),
                "container runtime effect may have committed: detail",
                pc::SandboxError::Transport(
                    "container runtime effect may have committed: detail".into(),
                ),
                Disposition::Transient,
                true,
            ),
            (
                || RuntimeError::EffectFenceExpired,
                "container runtime effect fence expired before the next provider boundary",
                pc::SandboxError::FenceRejected(
                    "container runtime effect fence expired before the next provider boundary"
                        .into(),
                ),
                Disposition::Permanent,
                true,
            ),
        ];
        for (make, display, original_port, disposition, stable) in rows {
            for wrappers in 0..=2 {
                let mut error = make();
                for _ in 0..wrappers {
                    error = error.after_mutation();
                }
                let uncertain = wrappers > 0 && !stable;
                let expected_display = if uncertain {
                    format!("container runtime effect may have committed: {display}")
                } else {
                    display.into()
                };
                assert_eq!(error.to_string(), expected_display, "{display}: {wrappers}");
                let expected = if uncertain {
                    pc::SandboxError::Transport(expected_display)
                } else {
                    original_port.clone()
                };
                let projected = pc::SandboxError::from(error);
                assert_eq!(projected, expected, "{display}: {wrappers}");
                assert_eq!(
                    projected.disposition(),
                    if uncertain {
                        Disposition::Transient
                    } else {
                        disposition
                    },
                    "{display}: {wrappers}",
                );
            }
        }
    }
}
