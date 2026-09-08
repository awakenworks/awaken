use std::io::Read;
use std::path::PathBuf;

use super::*;

/// Rereads the kubelet-rotated projected token on each request. The same object
/// implements bootstrap and registration-bound requests through the existing
/// authorizer port; there is no second registration or credential cache.
#[derive(Clone)]
pub struct KubernetesWorkerRequestAuthorizer {
    pod: KubernetesPodIdentity,
    token_file: PathBuf,
    identity: Option<WorkerIdentity>,
}

impl KubernetesWorkerRequestAuthorizer {
    /// Downward API metadata is only a requested coordinate. The Coordinator
    /// independently verifies it with TokenReview on every authenticated call.
    pub fn from_projected_directory(directory: &std::path::Path) -> Result<Self, String> {
        if directory.as_os_str().is_empty() {
            return Err("Kubernetes Worker identity directory must not be empty".into());
        }
        let read = |name: &str| -> Result<String, String> {
            let mut value = String::new();
            std::fs::File::open(directory.join(name))
                .and_then(|file| file.take(1025).read_to_string(&mut value))
                .map_err(|_| "Kubernetes Worker identity projection is unavailable".to_string())?;
            if value.len() > 1024 {
                return Err("Kubernetes Worker identity projection is too large".into());
            }
            Ok(value.trim().into())
        };
        Self::new(
            KubernetesPodIdentity::new(&read("namespace")?, &read("name")?, &read("uid")?)?,
            directory.join("token"),
        )
    }

    pub fn pod(&self) -> &KubernetesPodIdentity {
        &self.pod
    }

    pub fn into_remote_upstream(
        self,
        base_url: &str,
        ca: Option<&[u8]>,
    ) -> Result<WorkerUpstream, String> {
        let worker_id = self.pod.worker_id();
        select_remote_worker_transport(
            classify_remote_endpoint(base_url),
            RemoteCredentialPosture::PodBoundToken,
            RemoteIdentityPosture::Exact,
        )
        .ok_or_else(|| {
            "Kubernetes Worker transport requires HTTPS without embedded authority or query"
                .to_string()
        })?;
        self.token()?;
        WorkerUpstream::new(base_url)
            .with_worker_id(worker_id)
            .with_request_authorizer(Arc::new(self))
            .with_server_ca(ca)
    }
    pub fn new(pod: KubernetesPodIdentity, token_file: PathBuf) -> Result<Self, String> {
        if token_file.as_os_str().is_empty() {
            return Err("Kubernetes Worker token file must not be empty".into());
        }
        Ok(Self {
            pod,
            token_file,
            identity: None,
        })
    }

    fn token(&self) -> Result<String, String> {
        let mut token = String::new();
        std::fs::File::open(&self.token_file)
            .and_then(|file| {
                file.take(MAX_TOKEN_BYTES as u64 + 1)
                    .read_to_string(&mut token)
            })
            .map_err(|_| "Kubernetes Worker token file is unavailable".to_string())?;
        let value = token.trim();
        if token.len() > MAX_TOKEN_BYTES
            || value.is_empty()
            || value.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err("Kubernetes Worker token file is invalid".into());
        }
        Ok(value.into())
    }
}

impl WorkerRequestAuthorizer for KubernetesWorkerRequestAuthorizer {
    fn authorize(
        &self,
        method: &str,
        path: &str,
        worker_id: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String> {
        if worker_id != self.pod.worker_id()
            || self
                .identity
                .as_ref()
                .is_some_and(|identity| identity.worker_id != worker_id)
        {
            return Err("request Worker identity does not match the Kubernetes Pod".into());
        }
        if self.identity.is_none() && !(method == "POST" && path == REGISTER_PATH) {
            return Err("Kubernetes Worker requires registration before execution requests".into());
        }
        let request = request
            .header(WORKER_ID_HEADER, worker_id)
            .bearer_auth(self.token()?);
        match &self.identity {
            Some(identity) => Ok(request.header(
                IDENTITY_HEADER,
                serde_json::to_string(identity)
                    .map_err(|_| "Worker identity encoding failed".to_string())?,
            )),
            None => Ok(request),
        }
    }

    fn bind_worker_identity(&self, identity: &WorkerIdentity) -> Arc<dyn WorkerRequestAuthorizer> {
        Arc::new(Self {
            identity: Some(identity.clone()),
            ..self.clone()
        })
    }
}
