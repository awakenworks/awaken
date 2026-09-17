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
            RuntimeError::Backend(cause) => Self::backend("container runtime", cause),
        }
    }
}

impl RuntimeError {
    /// Classify a backend API response by its HTTP status. Only the statuses
    /// callers act on get their own class; every other response stays a
    /// mechanism failure.
    #[cfg(any(feature = "docker", feature = "k8s"))]
    pub(crate) fn from_api_status(status: u16, detail: String) -> Self {
        match status {
            404 => Self::NotFound(detail),
            409 => Self::Conflict(detail),
            401 | 403 => Self::Unauthorized(detail),
            _ => Self::Backend(detail),
        }
    }

    /// Classify an error observed after this invocation crossed its first
    /// backend mutation boundary. The wrapper is idempotent so nested runtime
    /// adapters cannot erase the stronger outcome.
    pub(crate) fn after_mutation(self) -> Self {
        match self {
            Self::MayHaveCommitted(_) | Self::EffectFenceExpired => self,
            error => Self::MayHaveCommitted(error.to_string()),
        }
    }
}

/// A failure a Docker or Kubernetes adapter turns into [`RuntimeError`].
///
/// Classification is chosen by the source type: an API response keeps the
/// class its HTTP status proves, while local mechanism failures (I/O, codec,
/// already-rendered detail) stay [`RuntimeError::Backend`]. A new API call
/// therefore cannot silently blanket a 404/409/401/403 into a retryable
/// backend failure.
#[cfg(any(feature = "docker", feature = "k8s"))]
pub(crate) trait RuntimeFailure {
    fn into_runtime_error(self) -> RuntimeError;
}

/// Local mechanism failures and already-rendered detail carry no API status.
#[cfg(any(feature = "docker", feature = "k8s"))]
macro_rules! mechanism_runtime_failure {
    ($($source:ty),+ $(,)?) => {
        $(impl RuntimeFailure for $source {
            fn into_runtime_error(self) -> RuntimeError {
                RuntimeError::Backend(self.to_string())
            }
        })+
    };
}

#[cfg(any(feature = "docker", feature = "k8s"))]
mechanism_runtime_failure!(
    &str,
    String,
    std::io::Error,
    serde_json::Error,
    pc::SandboxError,
    awaken_connection_plan::ConnectError,
);

#[cfg(feature = "k8s")]
mechanism_runtime_failure!(kube::config::InferConfigError);

#[cfg(feature = "docker")]
impl RuntimeFailure for bollard::errors::Error {
    fn into_runtime_error(self) -> RuntimeError {
        match self {
            bollard::errors::Error::DockerResponseServerError {
                status_code,
                message,
            } => RuntimeError::from_api_status(
                status_code,
                format!("Docker responded with status code {status_code}: {message}"),
            ),
            bollard::errors::Error::DockerStreamError { error } => RuntimeError::Backend(error),
            other => RuntimeError::Backend(other.to_string()),
        }
    }
}

#[cfg(feature = "k8s")]
impl RuntimeFailure for kube::Error {
    fn into_runtime_error(self) -> RuntimeError {
        match self {
            kube::Error::Api(response) => {
                let status = response.code;
                RuntimeError::from_api_status(status, kube::Error::Api(response).to_string())
            }
            other => RuntimeError::Backend(other.to_string()),
        }
    }
}

#[cfg(all(test, any(feature = "docker", feature = "k8s")))]
mod api_status_classification_tests {
    use super::*;

    fn port_class(error: RuntimeError) -> &'static str {
        match pc::SandboxError::from(error) {
            pc::SandboxError::NotFound(_) => "NotFound",
            pc::SandboxError::Conflict(_) => "Conflict",
            pc::SandboxError::Unauthorized(_) => "Unauthorized",
            pc::SandboxError::Backend { .. } => "Backend",
            other => panic!("unexpected class for an API response: {other:?}"),
        }
    }

    #[cfg(feature = "docker")]
    #[test]
    fn docker_api_status_keeps_the_class_callers_act_on() {
        // Decision table. C1 Docker Engine response status / non-response
        // failure. E1 port class. R1 404 absence; R2 409 state conflict;
        // R3 401/403 authority refusal; R4 every other status, stream error, or
        // transport failure stays a retryable mechanism failure.
        let response = |status_code: u16| bollard::errors::Error::DockerResponseServerError {
            status_code,
            message: "engine said no".into(),
        };
        let rows = [
            ("R1", response(404), "NotFound"),
            ("R2", response(409), "Conflict"),
            ("R3", response(401), "Unauthorized"),
            ("R3", response(403), "Unauthorized"),
            ("R4", response(500), "Backend"),
            ("R4", response(503), "Backend"),
            (
                "R4",
                bollard::errors::Error::DockerStreamError {
                    error: "pull aborted".into(),
                },
                "Backend",
            ),
            ("R4", bollard::errors::Error::RequestTimeoutError, "Backend"),
        ];
        for (rule, source, expected) in rows {
            let rendered = source.to_string();
            let runtime = source.into_runtime_error();
            let detail = runtime.to_string();
            assert_eq!(port_class(runtime), expected, "{rule}: {rendered}");
            if expected != "Backend" {
                assert!(detail.contains("engine said no"), "{rule}: {detail}");
            }
        }
    }

    #[cfg(feature = "k8s")]
    #[test]
    fn kubernetes_api_status_keeps_the_class_callers_act_on() {
        // Decision table. C1 kube-apiserver `ErrorResponse.code` / non-API
        // failure. R1 404 absence; R2 409 conflict; R3 401/403 authority
        // refusal; R4 every other status or client failure stays Backend.
        let response = |code: u16| {
            kube::Error::Api(kube::core::ErrorResponse {
                status: "Failure".into(),
                message: "apiserver said no".into(),
                reason: "Reason".into(),
                code,
            })
        };
        let rows = [
            ("R1", response(404), "NotFound"),
            ("R2", response(409), "Conflict"),
            ("R3", response(401), "Unauthorized"),
            ("R3", response(403), "Unauthorized"),
            ("R4", response(422), "Backend"),
            ("R4", response(500), "Backend"),
            (
                "R4",
                kube::Error::LinesCodecMaxLineLengthExceeded,
                "Backend",
            ),
        ];
        for (rule, source, expected) in rows {
            let runtime = source.into_runtime_error();
            let detail = runtime.to_string();
            assert_eq!(port_class(runtime), expected, "{rule}: {detail}");
            if expected != "Backend" {
                assert!(detail.contains("apiserver said no"), "{rule}: {detail}");
            }
        }
    }

    #[test]
    fn mechanism_failures_and_post_mutation_uncertainty_are_not_reclassified() {
        // M1 rendered local failures stay Backend; M2 a classified API failure
        // observed after a mutation boundary still becomes MayHaveCommitted,
        // because absence/conflict after a possible effect is not proof.
        assert_eq!(port_class("io".into_runtime_error()), "Backend", "M1");
        assert!(matches!(
            RuntimeError::from_api_status(409, "taken".into()).after_mutation(),
            RuntimeError::MayHaveCommitted(_)
        ));
    }
}
