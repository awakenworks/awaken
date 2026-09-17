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
