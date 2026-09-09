//! Pod-bound identity adapter for the existing Worker transport ports.
//!
//! TokenReview (not decoded JWT claims) supplies the Pod name and immutable UID.
//! This authenticates a caller; it neither attests private process confinement
//! nor proves termination. Dispatch still verifies the registered incarnation.
//! Deployment must select this authenticator with its Kubernetes binding source,
//! require TLS and isolate the projected token from untrusted workloads.

use std::time::Duration;

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec, TokenReviewStatus};
use kube::{Api, Client, api::PostParams};

use super::*;

mod authorizer;
pub use authorizer::KubernetesWorkerRequestAuthorizer;

const IDENTITY_HEADER: &str = "x-awaken-worker-incarnation";
const REGISTER_PATH: &str = "/v1/worker/register";
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

    /// One Pod UID has one incarnation. Registry still validates its allocated
    /// generation; a new random boot id cannot reopen this physical domain.
    pub fn matches_worker(&self, identity: &WorkerIdentity) -> bool {
        identity.worker_id == self.worker_id()
            && identity.incarnation_id == self.uid
            && identity.generation > 0
    }
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

/// One configured API-server trust domain and one allowed ServiceAccount.
/// No positive-result cache: Pod deletion/replacement must be rechecked by the
/// API server. A failed observation never falls back to header authentication.
pub struct KubernetesWorkerAuthenticator {
    reviews: Api<TokenReview>,
    namespace: String,
    service_account: String,
    audience: String,
    timeout: Duration,
}

impl KubernetesWorkerAuthenticator {
    pub fn new(
        client: Client,
        namespace: &str,
        service_account: &str,
        audience: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::validate_policy(namespace, service_account, audience, timeout)?;
        Ok(Self {
            reviews: Api::all(client),
            namespace: namespace.into(),
            service_account: service_account.into(),
            audience: audience.into(),
            timeout,
        })
    }

    /// Validate startup policy without connecting to an API server.
    pub fn validate_policy(
        namespace: &str,
        service_account: &str,
        audience: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        if !dns_name(namespace, 63)
            || namespace.contains('.')
            || !dns_name(service_account, 253)
            || audience.trim().is_empty()
            || audience.trim() != audience
            || timeout.is_zero()
        {
            return Err("invalid Kubernetes Worker authentication policy".into());
        }
        Ok(())
    }

    fn verified_pod(
        &self,
        status: &TokenReviewStatus,
    ) -> Result<KubernetesPodIdentity, WorkerAuthError> {
        let invalid = || invalid("Kubernetes token is not authorized for this Worker deployment");
        if status.authenticated != Some(true)
            || status.error.is_some()
            || !status
                .audiences
                .as_ref()
                .is_some_and(|v| v.contains(&self.audience))
        {
            return Err(invalid());
        }
        let user = status.user.as_ref().ok_or_else(invalid)?;
        let username = format!(
            "system:serviceaccount:{}:{}",
            self.namespace, self.service_account
        );
        if user.username.as_deref() != Some(username.as_str())
            || user.uid.as_deref().is_none_or(str::is_empty)
        {
            return Err(invalid());
        }
        let extra = user.extra.as_ref().ok_or_else(invalid)?;
        let single = |key: &str| -> Result<&str, WorkerAuthError> {
            match extra.get(key).map(Vec::as_slice) {
                Some([value]) => Ok(value),
                _ => Err(invalid()),
            }
        };
        KubernetesPodIdentity::new(
            &self.namespace,
            single("authentication.kubernetes.io/pod-name")?,
            single("authentication.kubernetes.io/pod-uid")?,
        )
        .map_err(|_| invalid())
    }
}

fn invalid(message: &str) -> WorkerAuthError {
    WorkerAuthError::Invalid(message.into())
}

fn single_header<'a>(parts: &'a Parts, name: &str) -> Result<Option<&'a str>, WorkerAuthError> {
    let mut values = parts.headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(invalid("duplicate Worker authentication header"));
    }
    value
        .map(|v| {
            v.to_str()
                .map_err(|_| invalid("invalid Worker authentication header"))
        })
        .transpose()
}

#[async_trait]
impl WorkerRequestAuthenticator for KubernetesWorkerAuthenticator {
    async fn authenticate(&self, parts: &Parts) -> Result<VerifiedWorkerContext, WorkerAuthError> {
        let worker_id = single_header(parts, WORKER_ID_HEADER)?.ok_or(WorkerAuthError::Missing)?;
        let requested_pod = KubernetesPodIdentity::from_worker_id(worker_id)
            .map_err(|_| invalid("invalid Kubernetes Worker id"))?;
        if requested_pod.namespace() != self.namespace {
            return Err(invalid("Worker belongs to another namespace"));
        }
        let identity: Option<WorkerIdentity> = single_header(parts, IDENTITY_HEADER)?
            .map(|value| {
                if value.len() > 4096 {
                    return Err(invalid("Worker identity header is too large"));
                }
                serde_json::from_str(value).map_err(|_| invalid("invalid Worker identity header"))
            })
            .transpose()?;
        match identity.as_ref() {
            Some(identity) if requested_pod.matches_worker(identity) => {}
            None if parts.method == "POST" && parts.uri.path() == REGISTER_PATH => {}
            _ => return Err(invalid("missing or mismatched Worker incarnation")),
        }
        let token = single_header(parts, "authorization")?
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|v| {
                !v.is_empty()
                    && v.len() <= MAX_TOKEN_BYTES
                    && !v.bytes().any(|b| b.is_ascii_whitespace())
            })
            .ok_or(WorkerAuthError::Missing)?;
        let review = TokenReview {
            spec: TokenReviewSpec {
                token: Some(token.into()),
                audiences: Some(vec![self.audience.clone()]),
            },
            ..Default::default()
        };
        // Never include response bodies or credential-bearing review objects in
        // errors. A timeout is authentication failure, not a cached grant.
        let reviewed = tokio::time::timeout(
            self.timeout,
            self.reviews.create(&PostParams::default(), &review),
        )
        .await
        .map_err(|_| invalid("Kubernetes token review timed out"))?
        .map_err(|_| invalid("Kubernetes token review unavailable"))?;
        let status = reviewed
            .status
            .as_ref()
            .ok_or_else(|| invalid("missing Kubernetes token review status"))?;
        let pod = self.verified_pod(status)?;
        if pod != requested_pod {
            return Err(invalid("Kubernetes token belongs to another Pod"));
        }
        let mut context = VerifiedWorkerContext::signed(
            worker_id.into(),
            identity,
            format!("k8s-pod:{}", pod.uid()),
        );
        // A non-restarting Pod is one execution domain. A process-proposed
        // nonce must not resurrect its closed Registry slot while an independent
        // controller stops that same Pod. Generation remains Registry-owned.
        context.registration_incarnation = Some(pod.uid().into());
        Ok(context)
    }
}

#[cfg(test)]
mod tests;
