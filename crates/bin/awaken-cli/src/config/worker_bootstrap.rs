use std::path::PathBuf;

use super::Role;

/// Process-local Worker bootstrap settings. Authority database and Control
/// sealing configuration deliberately live outside this boundary type.
#[derive(Debug, Clone)]
pub struct WorkerBootstrap {
    pub worker_id: String,
    /// Projected request-signing credential read only by the Worker root.
    pub request_credential_file: Option<PathBuf>,
    pub credential_material_root: PathBuf,
    pub credential_trust_domain: String,
    pub admin_listen: Option<String>,
    pub drain_grace_secs: u64,
    pub build_digest: Option<String>,
    pub zone: Option<String>,
    pub capabilities: Vec<String>,
    pub max_concurrent: Option<u32>,
    pub credential_probe_interval_secs: u64,
    pub credential_observation_ttl_secs: u64,
}

pub(super) fn validate_credential_file_ownership(
    role: Role,
    request_credential_file: bool,
    trust_credentials_file: bool,
) -> Result<(), String> {
    if role == Role::Worker && trust_credentials_file {
        return Err("worker_trust_credentials_file is owned by Coordinator, not Worker".to_owned());
    }
    if role != Role::Worker && request_credential_file {
        return Err(
            "worker_request_credential_file is owned by Worker, not this process role".to_owned(),
        );
    }
    Ok(())
}
