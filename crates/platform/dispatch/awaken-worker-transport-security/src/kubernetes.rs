// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! Pod-bound identity adapter for the existing Worker transport ports.
//!
//! TokenReview (not decoded JWT claims) supplies the Pod name and immutable UID.
//! This authenticates a caller; it neither attests private process confinement
//! nor proves termination. Dispatch still verifies the registered incarnation.
//! Deployment must select this authenticator with its Kubernetes binding source,
//! require TLS and isolate the projected token from untrusted workloads.

use awaken_worker_contract::WorkerIdentity;

#[cfg(feature = "kubernetes")]
mod authenticator;
#[cfg(feature = "kubernetes")]
mod authorizer;
#[cfg(feature = "kubernetes")]
pub use authenticator::KubernetesWorkerAuthenticator;
#[cfg(feature = "kubernetes")]
pub use authorizer::KubernetesWorkerRequestAuthorizer;

#[cfg(feature = "kubernetes")]
const IDENTITY_HEADER: &str = "x-awaken-worker-incarnation";
#[cfg(feature = "kubernetes")]
const REGISTER_PATH: &str = "/v1/worker/register";
#[cfg(feature = "kubernetes")]
const MAX_TOKEN_BYTES: usize = 32 * 1024;

/// A lookup coordinate, not authentication or a physical-stop receipt. The same
/// codec is used by startup, TokenReview validation and the binding adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KubernetesPodIdentity {
    namespace: String,
    name: String,
    uid: String,
}

impl KubernetesPodIdentity {
    pub fn new(namespace: &str, name: &str, uid: &str) -> Result<Self, String> {
        if !dns_name(namespace, 63)
            || namespace.contains('.')
            || !dns_name(name, 253)
            || uid.is_empty()
            || uid.len() > 128
            || !uid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("invalid Kubernetes Pod identity coordinate".into());
        }
        Ok(Self {
            namespace: namespace.into(),
            name: name.into(),
            uid: uid.into(),
        })
    }

    pub fn from_worker_id(worker_id: &str) -> Result<Self, String> {
        if worker_id.len() > 512 {
            return Err("Kubernetes Worker id is too large".into());
        }
        let fields: Vec<_> = worker_id.splitn(5, ':').collect();
        match fields.as_slice() {
            ["k8s", namespace, name, uid] => Self::new(namespace, name, uid),
            _ => Err("Worker id is not an exact Kubernetes Pod coordinate".into()),
        }
    }

    pub fn worker_id(&self) -> String {
        format!("k8s:{}:{}:{}", self.namespace, self.name, self.uid)
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// The Worker ID is this Pod; an incarnation is one container instance of
    /// it (a lowercase-hex container token), or, for incarnations registered
    /// before container identity, the Pod UID. Registry still validates the
    /// exact incarnation and generation; the binding source ties the token to
    /// the observed container.
    pub fn matches_worker(&self, identity: &WorkerIdentity) -> bool {
        identity.worker_id == self.worker_id()
            && (identity.incarnation_id == self.uid
                || is_container_incarnation(&identity.incarnation_id))
            && identity.generation > 0
    }
}

/// A container-instance incarnation token: 1..=128 lowercase hex digits. It
/// can never equal a Pod UID, which contains dashes.
pub fn is_container_incarnation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn dns_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[cfg(test)]
mod tests;
