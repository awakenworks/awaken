//! Docker source-type classification for the canonical container failure value.

use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;

/// A failure the Docker adapter turns into [`RuntimeError`].
///
/// Classification is chosen by the source type: an API response keeps the
/// class its HTTP status proves, while local mechanism failures (I/O, codec,
/// already-rendered detail) stay [`RuntimeError::Backend`]. A new API call
/// therefore cannot silently blanket a 404/409/401/403 into a retryable
/// backend failure.
#[cfg(feature = "docker")]
pub(crate) trait RuntimeFailure {
    fn into_runtime_error(self) -> RuntimeError;
}

/// Local mechanism failures and already-rendered detail carry no API status.
#[cfg(feature = "docker")]
macro_rules! mechanism_runtime_failure {
    ($($source:ty),+ $(,)?) => {
        $(impl RuntimeFailure for $source {
            fn into_runtime_error(self) -> RuntimeError {
                RuntimeError::Backend(self.to_string())
            }
        })+
    };
}

#[cfg(feature = "docker")]
mechanism_runtime_failure!(
    &str,
    String,
    std::io::Error,
    serde_json::Error,
    pc::SandboxError,
    awaken_connection_plan::ConnectError,
);

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

#[cfg(all(test, feature = "docker"))]
mod api_status_classification_tests {
    use super::*;

    fn port_class(error: RuntimeError) -> &'static str {
        match pc::SandboxError::from(error) {
            pc::SandboxError::NotFound(_) => "NotFound",
            pc::SandboxError::Conflict(_) => "Conflict",
            pc::SandboxError::Unauthorized(_) => "Unauthorized",
            pc::SandboxError::CapacityUnavailable(_) => "CapacityUnavailable",
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

    #[test]
    fn mechanism_failures_and_post_mutation_uncertainty_are_not_reclassified() {
        // M1 rendered local failures stay Backend; M2 a classified API failure
        // observed after a mutation boundary still becomes MayHaveCommitted,
        // because absence/conflict after a possible effect is not proof.
        assert_eq!(port_class("io".into_runtime_error()), "Backend", "M1");
        // E3: rendered status/quota-looking mechanism text has no typed API
        // status. Docker's real source trait must keep Backend; a genuine
        // Docker 403 keeps Unauthorized even when its detail mentions quota.
        for detail in ["HTTP 404 NotFound", "403 exceeded quota", "409 conflict"] {
            assert_eq!(port_class(detail.into_runtime_error()), "Backend", "E3");
        }
        assert_eq!(
            port_class(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 403,
                    message: "exceeded quota".into(),
                }
                .into_runtime_error()
            ),
            "Unauthorized",
            "E3: Kubernetes quota overlay does not classify Docker",
        );
        assert!(matches!(
            RuntimeError::from_api_status(409, "taken".into()).after_mutation(),
            RuntimeError::MayHaveCommitted(_)
        ));
    }
}
