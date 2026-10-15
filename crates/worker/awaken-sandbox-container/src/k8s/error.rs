//! Kubernetes API error classification shared by runtime submodules.

use crate::RuntimeError;

pub(crate) fn backend(error: impl crate::runtime::RuntimeFailure) -> RuntimeError {
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
