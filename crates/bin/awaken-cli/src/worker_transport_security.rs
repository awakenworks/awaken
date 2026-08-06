//! Projected-file adapters for the existing signed Worker transport contract.
//!
//! These are composition-only DTOs. The authoritative authentication behavior
//! remains in `awaken-runtime-host`; this module only turns deployment files into
//! its existing client and server ports.

use std::path::Path;
use std::sync::Arc;

use crate::config::{OperatingMode, ResolvedDeployment};

fn read_projected_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| {
        format!(
            "read Worker transport credential {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn authenticator(
    deployment: &ResolvedDeployment,
) -> Result<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>, String> {
    let Some(path) = deployment.worker_trust_credentials_file.as_deref() else {
        return match deployment.mode {
            OperatingMode::Local => Ok(Arc::new(
                awaken_worker_transport_security::HeaderWorkerAuthenticator,
            )),
            OperatingMode::Server => Err(
                "server-mode Coordinator/AllInOne requires worker_trust_credentials_file"
                    .to_owned(),
            ),
        };
    };
    let credentials = awaken_worker_transport_security::parse_projected_signing_credentials(
        &read_projected_file(path)?,
    )?;
    let mut credentials = credentials.into_iter().map(Ok::<_, String>);
    let first = credentials
        .next()
        .ok_or_else(|| "Worker trust credentials must contain at least one entry".to_owned())??;
    let authenticator = awaken_worker_transport_security::SignedWorkerAuthenticator::new(first);
    for credential in credentials {
        authenticator.enroll(credential?);
    }
    Ok(Arc::new(authenticator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[tokio::test]
    async fn coordinator_transport_posture_follows_the_deployment_decision_table() {
        // Causes: C1 operating mode Local/Server; C2 trust file absent/present;
        // C3 enrollment empty/valid. Effects: E1 Local may use the compatibility
        // header; E2 Server fails startup without trust; E3 empty trust fails;
        // E4 valid trust installs signed authentication and rejects a bare header.
        //
        // | Rule | C1     | C2      | C3    | Effect |
        // | R1   | Local  | absent  | -     | E1     |
        // | R2   | Server | absent  | -     | E2     |
        // | R3   | Server | present | empty | E3     |
        // | R4   | Server | present | valid | E4     |
        let dir = tempfile::tempdir().unwrap();
        let mut deployment = config::local_test_deployment(dir.path().to_path_buf());

        let local = authenticator(&deployment).expect("R1 local compatibility posture");
        let local_parts = axum::http::Request::builder()
            .uri("/v1/worker/register")
            .header(
                awaken_worker_transport_security::WORKER_ID_HEADER,
                "worker-local",
            )
            .body(())
            .unwrap()
            .into_parts()
            .0;
        assert!(local.authenticate(&local_parts).await.is_ok(), "R1");

        deployment.mode = OperatingMode::Server;
        assert!(
            authenticator(&deployment)
                .err()
                .expect("R2 missing trust error")
                .contains("requires"),
            "R2"
        );

        let trust = dir.path().join("trust.json");
        std::fs::write(&trust, "[]").unwrap();
        deployment.worker_trust_credentials_file = Some(trust.clone());
        assert!(
            authenticator(&deployment)
                .err()
                .expect("R3 empty trust error")
                .contains("at least one"),
            "R3"
        );

        std::fs::write(
            &trust,
            r#"[
              {"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0LWE="},
              {"worker_id":"worker-b","key_id":"key-b","credential_id":"credential-b","secret_base64":"c2VjcmV0LWI="}
            ]"#,
        )
        .unwrap();
        let signed = authenticator(&deployment).expect("R4 signed posture");
        assert!(signed.authenticate(&local_parts).await.is_err(), "R4");
    }
}
