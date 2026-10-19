//! Kubernetes API error classification shared by runtime submodules.

use awaken_provisioning_contract as pc;

use awaken_container_contract::RuntimeError;

/// A failure the Kubernetes adapter turns into [`RuntimeError`].
///
/// Classification is chosen by the source type: an API response keeps the
/// class its HTTP status proves, while local mechanism failures (I/O, codec,
/// already-rendered detail) stay [`RuntimeError::Backend`]. A new API call
/// therefore cannot silently blanket a 404/409/401/403 into a retryable
/// backend failure.
pub(crate) trait KubernetesFailure {
    fn into_runtime_error(self) -> RuntimeError;
}

/// Local mechanism failures and already-rendered detail carry no API status.
macro_rules! mechanism_failure {
    ($($source:ty),+ $(,)?) => {
        $(impl KubernetesFailure for $source {
            fn into_runtime_error(self) -> RuntimeError {
                RuntimeError::Backend(self.to_string())
            }
        })+
    };
}

mechanism_failure!(
    &str,
    String,
    std::io::Error,
    serde_json::Error,
    pc::SandboxError,
    awaken_connection_plan::ConnectError,
    kube::config::InferConfigError,
);

impl KubernetesFailure for kube::Error {
    fn into_runtime_error(self) -> RuntimeError {
        match self {
            // Only the quota admission's refusal means "no room now"; every
            // other refusal keeps its status class (ADR-0092 decision 1).
            kube::Error::Api(response) => {
                let (status, message) = (response.code, response.message.clone());
                let detail = kube::Error::Api(response).to_string();
                if awaken_container_contract::kubernetes_refusal_is_capacity(status, &message) {
                    RuntimeError::CapacityUnavailable(detail)
                } else {
                    RuntimeError::from_api_status(status, detail)
                }
            }
            other => RuntimeError::Backend(other.to_string()),
        }
    }
}

pub(crate) fn backend(error: impl KubernetesFailure) -> RuntimeError {
    error.into_runtime_error()
}

pub(crate) fn api_conflict(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 409)
}

pub(super) fn api_not_found(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 404)
}

/// An incomplete Status cannot be collapsed into object absence by the SDK.
/// Every optional Sandbox read uses this one existing API/error boundary.
pub(crate) async fn get_optional<K>(
    api: &kube::Api<K>,
    name: &str,
) -> Result<Option<K>, RuntimeError>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.get(name).await {
        Ok(object) => Ok(Some(object)),
        Err(kube::Error::Api(response)) if response.status.is_none() => Err(backend(
            <serde_json::Error as serde::de::Error>::missing_field("status"),
        )),
        Err(kube::Error::Api(response))
            if response.is_failure() && response.reason == "NotFound" =>
        {
            Ok(None)
        }
        Err(error) => Err(backend(error)),
    }
}

#[cfg(test)]
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

    #[test]
    fn kubernetes_api_status_keeps_the_class_callers_act_on() {
        // Decision table. C1 kube-apiserver Status.code / non-API
        // failure. R1 404 absence; R2 409 conflict; R3 401/403 authority
        // refusal; R4 every other status or client failure stays Backend.
        let response = |code: u16| {
            kube::Error::Api(Box::new(
                kube::core::Status::failure("apiserver said no", "Reason").with_code(code),
            ))
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
    fn kubernetes_quota_exhaustion_is_capacity_and_every_other_refusal_keeps_its_class() {
        // Decision table (ADR-0092 decision 1). Causes: C1 HTTP status; C2 the
        // `Status` message carries the quota admission's `exceeded quota`
        // marker. Effects: E1 CapacityUnavailable (Transient at the port, see
        // the provisioning-contract construction table); E2 Unauthorized; E3
        // the existing status class.
        // Q1 403+C2 => E1; Q2 403 from RBAC, the broker's own `Forbidden`, a
        // quota that demands explicit requests (`failed quota: … must
        // specify`) or a LimitRange => E2; Q3 401 => E2 even with the marker;
        // Q4 404/409/422/500 => E3 even with the marker.
        let quota = "pods \"awaken-s\" is forbidden: exceeded quota: compute, requested: \
                     requests.cpu=2, used: requests.cpu=4, limited: requests.cpu=4";
        let rows = [
            ("Q1", 403, quota, "CapacityUnavailable"),
            (
                "Q2 RBAC",
                403,
                "pods is forbidden: User \"system:serviceaccount:w:w\" cannot create \
                 resource \"pods\" in API group \"\" in the namespace \"s\"",
                "Unauthorized",
            ),
            ("Q2 broker stale Worker", 403, "Forbidden", "Unauthorized"),
            (
                "Q2 quota requires requests",
                403,
                "pods \"awaken-s\" is forbidden: failed quota: compute: must specify \
                 limits.cpu for: agent",
                "Unauthorized",
            ),
            (
                "Q2 LimitRange",
                403,
                "pods \"awaken-s\" is forbidden: maximum cpu usage per Container is 2, \
                 but limit is 4",
                "Unauthorized",
            ),
            ("Q3", 401, quota, "Unauthorized"),
            ("Q4", 404, quota, "NotFound"),
            ("Q4", 409, quota, "Conflict"),
            ("Q4", 422, quota, "Backend"),
            ("Q4", 500, quota, "Backend"),
        ];
        for (rule, status, message, expected) in rows {
            let response = kube::Error::Api(Box::new(
                kube::core::Status::failure(message, "Forbidden").with_code(status),
            ));
            let runtime = response.into_runtime_error();
            let port = pc::SandboxError::from(runtime);
            assert_eq!(port_class_of(&port), expected, "{rule}: {port}");
            if expected == "CapacityUnavailable" {
                assert!(port.detail().contains("exceeded quota"), "{rule}: {port}");
            }
        }
        // Q5 a refused Create had no effect: the post-mutation wrapper keeps
        // the capacity class instead of turning it into MayHaveCommitted.
        assert!(matches!(
            RuntimeError::CapacityUnavailable(quota.into()).after_mutation(),
            RuntimeError::CapacityUnavailable(_)
        ));
    }

    fn port_class_of(error: &pc::SandboxError) -> &'static str {
        match error {
            pc::SandboxError::CapacityUnavailable(_) => "CapacityUnavailable",
            pc::SandboxError::NotFound(_) => "NotFound",
            pc::SandboxError::Conflict(_) => "Conflict",
            pc::SandboxError::Unauthorized(_) => "Unauthorized",
            pc::SandboxError::Backend { .. } => "Backend",
            other => panic!("unexpected class for an API response: {other:?}"),
        }
    }
}
