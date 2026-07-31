//! Projected-file adapters for the existing signed Worker transport contract.
//!
//! These are composition-only DTOs. The authoritative authentication behavior
//! remains in `awaken-runtime-host`; this module only turns deployment files into
//! its existing client and server ports.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde::Deserialize;

use crate::config::{OperatingMode, ResolvedDeployment};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectedWorkerCredential {
    worker_id: String,
    key_id: String,
    credential_id: String,
    secret_base64: String,
}

impl ProjectedWorkerCredential {
    fn into_signing_credential(
        self,
    ) -> Result<awaken_runtime_host::WorkerSigningCredential, String> {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(self.secret_base64.trim())
            .map_err(|_| "worker transport credential secret_base64 is invalid".to_owned())?;
        awaken_runtime_host::WorkerSigningCredential::new(
            self.worker_id,
            self.key_id,
            self.credential_id,
            secret,
        )
        .map_err(|error| error.to_string())
    }
}

fn read_projected_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| {
        format!(
            "read Worker transport credential {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn request_authorizer(
    deployment: &ResolvedDeployment,
) -> Result<Option<Arc<dyn awaken_runtime_host::WorkerRequestAuthorizer>>, String> {
    let Some(path) = deployment.worker.request_credential_file.as_deref() else {
        return match deployment.mode {
            OperatingMode::Local => Ok(None),
            OperatingMode::Server => {
                Err("server-mode Worker requires worker_request_credential_file".to_owned())
            }
        };
    };
    let projected: ProjectedWorkerCredential = serde_json::from_str(&read_projected_file(path)?)
        .map_err(|error| {
            format!(
                "parse Worker request credential {}: {error}",
                path.display()
            )
        })?;
    let credential = projected.into_signing_credential()?;
    if credential.worker_id() != deployment.worker.worker_id {
        return Err("Worker request credential does not match configured worker_id".to_owned());
    }
    Ok(Some(Arc::new(
        awaken_runtime_host::SignedWorkerRequestAuthorizer::new(credential),
    )))
}

pub(crate) fn authenticator(
    deployment: &ResolvedDeployment,
) -> Result<Arc<dyn awaken_runtime_host::WorkerRequestAuthenticator>, String> {
    let Some(path) = deployment.worker_trust_credentials_file.as_deref() else {
        return match deployment.mode {
            OperatingMode::Local => Ok(Arc::new(awaken_runtime_host::HeaderWorkerAuthenticator)),
            OperatingMode::Server => Err(
                "server-mode Coordinator/AllInOne requires worker_trust_credentials_file"
                    .to_owned(),
            ),
        };
    };
    let projected: Vec<ProjectedWorkerCredential> =
        serde_json::from_str(&read_projected_file(path)?).map_err(|error| {
            format!("parse Worker trust credentials {}: {error}", path.display())
        })?;
    let mut credentials = projected
        .into_iter()
        .map(ProjectedWorkerCredential::into_signing_credential);
    let first = credentials
        .next()
        .ok_or_else(|| "Worker trust credentials must contain at least one entry".to_owned())??;
    let authenticator = awaken_runtime_host::SignedWorkerAuthenticator::new(first);
    for credential in credentials {
        authenticator.enroll(credential?);
    }
    Ok(Arc::new(authenticator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[test]
    fn projected_worker_transport_credentials_fail_closed() {
        // Cause/effect decision table:
        // R1 Local + no file -> compatibility authorizer is omitted.
        // R2 Server + no file -> startup fails before any request is sent.
        // R3 File identity differs from worker_id -> startup fails closed.
        // R4 Exact identity + valid base64 secret -> signed authorizer is installed.
        let dir = tempfile::tempdir().unwrap();
        let mut deployment = config::worker_test_deployment(dir.path().to_path_buf());
        assert!(request_authorizer(&deployment).unwrap().is_none(), "R1");

        deployment.mode = OperatingMode::Server;
        assert!(
            request_authorizer(&deployment)
                .err()
                .expect("R2 error")
                .contains("requires"),
            "R2"
        );

        let credential = dir.path().join("worker.json");
        std::fs::write(
            &credential,
            r#"{"worker_id":"other","key_id":"key-1","credential_id":"credential-1","secret_base64":"c2VjcmV0"}"#,
        )
        .unwrap();
        deployment.worker.request_credential_file = Some(credential.clone());
        assert!(
            request_authorizer(&deployment)
                .err()
                .expect("R3 error")
                .contains("worker_id"),
            "R3"
        );

        std::fs::write(
            &credential,
            r#"{"worker_id":"awaken-worker","key_id":"key-1","credential_id":"credential-1","secret_base64":"c2VjcmV0"}"#,
        )
        .unwrap();
        assert!(request_authorizer(&deployment).unwrap().is_some(), "R4");
    }

    #[test]
    fn coordinator_trust_file_requires_at_least_one_valid_credential() {
        // Causes: Server mode with no enrollment, an empty enrollment array, and
        // two valid enrolled Workers. Effects: the first two fail startup; the
        // final rule constructs the one shared signed authenticator.
        let dir = tempfile::tempdir().unwrap();
        let mut deployment = config::local_test_deployment(dir.path().to_path_buf());
        deployment.mode = OperatingMode::Server;
        assert!(
            authenticator(&deployment)
                .err()
                .expect("missing trust error")
                .contains("requires")
        );

        let trust = dir.path().join("trust.json");
        std::fs::write(&trust, "[]").unwrap();
        deployment.worker_trust_credentials_file = Some(trust.clone());
        assert!(
            authenticator(&deployment)
                .err()
                .expect("empty trust error")
                .contains("at least one")
        );

        std::fs::write(
            &trust,
            r#"[
              {"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0LWE="},
              {"worker_id":"worker-b","key_id":"key-b","credential_id":"credential-b","secret_base64":"c2VjcmV0LWI="}
            ]"#,
        )
        .unwrap();
        assert!(authenticator(&deployment).is_ok());
    }
}
