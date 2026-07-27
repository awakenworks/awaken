//! Explicit data-plane dependencies used by a database-less Worker.

/// Resource-plane wiring for a database-less Worker.
///
/// The ports remain authoritative data-plane dependencies; the Worker only
/// materializes their already-authorized bindings for an attempt.
pub struct WorkerResourcePlane {
    pub(crate) ports: awaken_runtime_host::ResourcePlanePorts,
    pub(crate) validator: awaken_server::ResourceBindingValidatorPort,
    pub(crate) credentials: Option<awaken_control::InferenceMaterializationStores>,
}

impl WorkerResourcePlane {
    #[must_use]
    pub fn new(
        ports: awaken_runtime_host::ResourcePlanePorts,
        validator: awaken_server::ResourceBindingValidatorPort,
    ) -> Self {
        Self {
            ports,
            validator,
            credentials: None,
        }
    }

    #[must_use]
    pub fn with_repository_credentials(
        mut self,
        credentials: awaken_control::InferenceMaterializationStores,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }

    pub(crate) fn supports_repository_credentials(&self) -> bool {
        self.credentials.is_some()
    }
}

pub(crate) async fn shared_resource_wiring(
    credentials: Option<awaken_control::InferenceMaterializationStores>,
    resource_url: Option<&str>,
    admin_backend: Option<&awaken_control::StoreBackend>,
) -> Result<Option<WorkerResourcePlane>, Box<dyn std::error::Error + Send + Sync>> {
    let ports = awaken_server::shared_worker_resource_plane(resource_url).await?;
    let validator = awaken_control::open_shared_resource_validator(admin_backend).await?;
    match (ports, validator) {
        (None, None) => Ok(None),
        (Some(ports), Some(validator)) => {
            let resources = WorkerResourcePlane::new(ports, validator);
            Ok(Some(match credentials {
                Some(credentials) => resources.with_repository_credentials(credentials),
                None => resources,
            }))
        }
        (Some(_), None) => Err(std::io::Error::other(
            "resource_database_url requires a shared admin store on a remote worker",
        )
        .into()),
        (None, Some(_)) => Err(std::io::Error::other(
            "a shared admin store requires resource_database_url on a resource worker",
        )
        .into()),
    }
}
