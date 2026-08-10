//! Typed configuration for the private Control-to-Coordinator registration edge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::Role;

pub(super) fn enforce_worker_database_isolation(
    role: Role,
    configured_databases: &[(&str, bool)],
) -> Result<(), String> {
    if role != Role::Worker {
        return Ok(());
    }
    let forbidden = configured_databases
        .iter()
        .filter_map(|(name, configured)| configured.then_some(*name))
        .collect::<Vec<_>>();
    if forbidden.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Worker must not receive authority database configuration: {}",
            forbidden.join(", ")
        ))
    }
}

pub(super) fn enforce_control_execution_database_isolation(
    role: Role,
    configured_databases: &[(&str, bool)],
) -> Result<(), String> {
    if role != Role::Control {
        return Ok(());
    }
    let forbidden = configured_databases
        .iter()
        .filter_map(|(name, configured)| configured.then_some(*name))
        .collect::<Vec<_>>();
    if forbidden.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Control must not receive Managed Execution database configuration: {}",
            forbidden.join(", ")
        ))
    }
}

pub(super) fn enforce_coordinator_control_database_isolation(
    role: Role,
    configured_databases: &[(&str, bool)],
) -> Result<(), String> {
    if role != Role::Coordinator {
        return Ok(());
    }
    let forbidden = configured_databases
        .iter()
        .filter_map(|(name, configured)| configured.then_some(*name))
        .collect::<Vec<_>>();
    if forbidden.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Coordinator must not receive Control database or seal-key configuration: {}",
            forbidden.join(", ")
        ))
    }
}

/// Role-aware inputs for the executable Agent registration adapters. The token
/// is projected as a file and loaded only by the process that owns the adapter;
/// it is never retained in the redacted deployment report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutableAgentRegistrationConfig {
    coordinator_url: Option<String>,
    token_file: Option<PathBuf>,
}

/// Role-owned credentials for Coordinator calls back into the Control
/// application ports. AllInOne uses local adapters and therefore owns neither
/// URL nor token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlServiceConfig {
    control_url: Option<String>,
    token_file: Option<PathBuf>,
}

impl ControlServiceConfig {
    pub(crate) fn resolve(
        role: Role,
        control_url: Option<String>,
        token_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        let control_url = control_url
            .map(|value| value.trim_end_matches('/').to_owned())
            .filter(|value| !value.trim().is_empty());
        if control_url
            .as_ref()
            .is_some_and(|value| !value.starts_with("http://") && !value.starts_with("https://"))
        {
            return Err("control_internal_url must use http:// or https://".into());
        }
        if token_file
            .as_deref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err("control_service_token_file must not be empty".into());
        }
        match role {
            Role::Control if control_url.is_some() || token_file.is_none() => Err(
                "Control requires control_service_token_file and must not configure control_internal_url"
                    .into(),
            ),
            Role::Coordinator if control_url.is_none() || token_file.is_none() => Err(
                "Coordinator requires control_internal_url and control_service_token_file".into(),
            ),
            Role::AllInOne | Role::Worker if control_url.is_some() || token_file.is_some() => Err(
                "control_internal_url and control_service_token_file belong only to split Control/Coordinator"
                    .into(),
            ),
            _ => Ok(Self {
                control_url,
                token_file,
            }),
        }
    }

    pub fn control_authenticator(
        &self,
    ) -> Result<Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>, String> {
        Ok(Arc::new(
            awaken_service_auth_contract::TokenSourceAuthenticator::new(projected_token_source(
                self.token_file.as_deref(),
                "control_service_token_file",
                "Control service",
            )?),
        ))
    }

    pub fn coordinator_credentials(
        &self,
    ) -> Result<
        (
            &str,
            Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
        ),
        String,
    > {
        let url = self
            .control_url
            .as_deref()
            .ok_or_else(|| "Coordinator requires control_internal_url".to_owned())?;
        Ok((
            url,
            projected_token_source(
                self.token_file.as_deref(),
                "control_service_token_file",
                "Control service",
            )?,
        ))
    }
}

impl ExecutableAgentRegistrationConfig {
    pub(crate) fn resolve(
        role: Role,
        coordinator_url: Option<String>,
        token_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        let (coordinator_url, token_file) = resolve_private_boundary(
            role,
            coordinator_url,
            token_file,
            "executable_agent_registration_token_file",
            "executable Agent registration",
        )?;
        Ok(Self {
            coordinator_url,
            token_file,
        })
    }

    pub fn control_credentials(
        &self,
    ) -> Result<
        (
            &str,
            Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
        ),
        String,
    > {
        let url = self.coordinator_url.as_deref().ok_or_else(|| {
            "Control requires coordinator_internal_url for executable Agent registration".to_owned()
        })?;
        Ok((
            url,
            projected_token_source(
                self.token_file.as_deref(),
                "executable_agent_registration_token_file",
                "executable Agent registration",
            )?,
        ))
    }

    pub fn coordinator_authenticator(
        &self,
    ) -> Result<Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>, String> {
        Ok(Arc::new(
            awaken_service_auth_contract::TokenSourceAuthenticator::new(projected_token_source(
                self.token_file.as_deref(),
                "executable_agent_registration_token_file",
                "executable Agent registration",
            )?),
        ))
    }
}

/// File projection adapter for Kubernetes Secret/workload-token rotation. The
/// path is stable while its contents are resolved for every request by the
/// shared service-auth contract.
#[derive(Clone)]
struct ProjectedFileServiceBearerTokenSource {
    path: PathBuf,
    boundary: &'static str,
}

impl awaken_service_auth_contract::ServiceBearerTokenSource
    for ProjectedFileServiceBearerTokenSource
{
    fn current_token(&self) -> Result<Arc<str>, String> {
        load_token(Some(&self.path), "token_file", self.boundary).map(Arc::from)
    }
}

fn projected_token_source(
    path: Option<&Path>,
    field: &str,
    boundary: &'static str,
) -> Result<Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>, String> {
    let path = path.ok_or_else(|| format!("{field} is required for split deployment"))?;
    let source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource> =
        Arc::new(ProjectedFileServiceBearerTokenSource {
            path: path.to_path_buf(),
            boundary,
        });
    awaken_service_auth_contract::resolve_service_bearer_token(source.as_ref())?;
    Ok(source)
}

fn resolve_private_boundary(
    role: Role,
    coordinator_url: Option<String>,
    token_file: Option<PathBuf>,
    token_field: &str,
    boundary: &str,
) -> Result<(Option<String>, Option<PathBuf>), String> {
    if role == Role::Worker && (coordinator_url.is_some() || token_file.is_some()) {
        return Err(format!(
            "Worker must not receive {boundary} endpoint credentials"
        ));
    }
    let coordinator_url = coordinator_url
        .map(|value| value.trim_end_matches('/').to_owned())
        .filter(|value| !value.trim().is_empty());
    if coordinator_url
        .as_ref()
        .is_some_and(|value| !value.starts_with("http://") && !value.starts_with("https://"))
    {
        return Err("coordinator_internal_url must use http:// or https://".into());
    }
    if token_file
        .as_deref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(format!("{token_field} must not be empty"));
    }
    if role != Role::Coordinator && coordinator_url.is_some() != token_file.is_some() {
        return Err(format!(
            "coordinator_internal_url and {token_field} must be configured together"
        ));
    }
    Ok((coordinator_url, token_file))
}

fn load_token(path: Option<&Path>, field: &str, boundary: &str) -> Result<String, String> {
    let path = path.ok_or_else(|| format!("{field} is required for split deployment"))?;
    let token = std::fs::read_to_string(path)
        .map_err(|error| format!("read {boundary} token {}: {error}", path.display()))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(format!("{boundary} token {} is empty", path.display()));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigOverrides, FileConfig, ResolvedDeployment};

    #[test]
    fn registration_configuration_is_role_scoped_and_complete() {
        // Cause/effect decision table: C1 Control complete pair -> accepted;
        // C2 Control partial pair -> rejected; C3 Coordinator token-only server
        // input -> accepted; C4 Worker receives either authority credential ->
        // rejected; C5 malformed scheme -> rejected before transport assembly.
        let token = PathBuf::from("/var/run/secrets/registration-token");
        assert!(
            ExecutableAgentRegistrationConfig::resolve(
                Role::Control,
                Some("http://coordinator:8080/".into()),
                Some(token.clone()),
            )
            .is_ok(),
            "C1"
        );
        assert!(
            ExecutableAgentRegistrationConfig::resolve(
                Role::Control,
                Some("http://coordinator:8080".into()),
                None,
            )
            .is_err(),
            "C2"
        );
        assert!(
            ExecutableAgentRegistrationConfig::resolve(
                Role::Coordinator,
                None,
                Some(token.clone()),
            )
            .is_ok(),
            "C3"
        );
        assert!(
            ExecutableAgentRegistrationConfig::resolve(
                Role::Worker,
                Some("http://coordinator:8080".into()),
                Some(token.clone()),
            )
            .is_err(),
            "C4"
        );
        assert!(
            ExecutableAgentRegistrationConfig::resolve(
                Role::Control,
                Some("coordinator:8080".into()),
                Some(token),
            )
            .is_err(),
            "C5"
        );
    }

    #[test]
    fn projected_token_is_loaded_trimmed_and_never_defaulted() {
        // Cause/effect decision table: R1 missing file and R2 empty file -> fail
        // closed; R3 a projected token plus newline -> trim and return it; R4
        // atomically replaced file contents -> the existing source observes the
        // successor without rebuilding CLI wiring; R5 a later empty projection
        // -> source error, never reuse the previous token. No environment or
        // startup-cache fallback exists.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(load_token(Some(&missing), "registration_token_file", "registration").is_err());
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "\n").unwrap();
        assert!(load_token(Some(&empty), "registration_token_file", "registration").is_err());
        let valid = dir.path().join("valid");
        std::fs::write(&valid, "secret-token\n").unwrap();
        assert_eq!(
            load_token(Some(&valid), "registration_token_file", "registration").unwrap(),
            "secret-token"
        );
        let source =
            projected_token_source(Some(&valid), "registration_token_file", "registration")
                .unwrap();
        assert_eq!(
            source.current_token().unwrap().as_ref(),
            "secret-token",
            "R3"
        );
        std::fs::write(&valid, "rotated-token\n").unwrap();
        assert_eq!(
            source.current_token().unwrap().as_ref(),
            "rotated-token",
            "R4"
        );
        std::fs::write(&valid, "\n").unwrap();
        assert!(source.current_token().is_err(), "R5");
    }

    #[test]
    fn split_boundary_credentials_share_request_time_file_semantics() {
        // Cause/effect decision table: R1 Control-side client credentials read
        // the first file value; R2 Coordinator-side router authentication reads
        // the same projection; R3 replacing the file makes the already-built
        // client source and authenticator reject the predecessor and accept the
        // successor. This pins the CLI composition methods rather than only the
        // generic token-source implementation.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-token");
        std::fs::write(&path, "first\n").unwrap();
        let control = ExecutableAgentRegistrationConfig {
            coordinator_url: Some("http://coordinator:8080".into()),
            token_file: Some(path.clone()),
        };
        let (_, client_source) = control.control_credentials().unwrap();
        let coordinator = ExecutableAgentRegistrationConfig {
            coordinator_url: None,
            token_file: Some(path.clone()),
        };
        let authenticator = coordinator.coordinator_authenticator().unwrap();
        let requirement = awaken_service_auth_contract::ServiceAuthorizationRequirement::new(
            awaken_service_auth_contract::COORDINATOR_SERVICE_AUDIENCE,
            "agent:publish",
        )
        .in_workspace("workspace-a");
        assert_eq!(
            client_source.current_token().unwrap().as_ref(),
            "first",
            "R1"
        );
        assert!(
            authenticator
                .authenticate(Some(b"Bearer first"), requirement)
                .is_ok(),
            "R2"
        );

        std::fs::write(&path, "second\n").unwrap();
        assert_eq!(
            client_source.current_token().unwrap().as_ref(),
            "second",
            "R3"
        );
        assert!(matches!(
            authenticator.authenticate(Some(b"Bearer first"), requirement),
            Err(awaken_service_auth_contract::ServiceAuthError::Unauthorized)
        ));
        assert!(
            authenticator
                .authenticate(Some(b"Bearer second"), requirement)
                .is_ok(),
            "R3"
        );
    }

    #[test]
    fn worker_rejects_every_authority_database_binding() {
        // Causes: Worker with no authority database versus one or several
        // Control/Coordinator/Resource database or Control seal-key settings.
        // Effects: the first is accepted; every configured authority field is
        // named in one fail-closed error before a connection or key read.
        assert!(enforce_worker_database_isolation(Role::Worker, &[]).is_ok());
        let error = enforce_worker_database_isolation(
            Role::Worker,
            &[("runtime_database_url", true), ("credential_db", true)],
        )
        .unwrap_err();
        assert!(error.contains("runtime_database_url"));
        assert!(error.contains("credential_db"));
        assert!(enforce_worker_database_isolation(Role::Control, &[("config_db", true)]).is_ok());

        let key_error = ResolvedDeployment::resolve_file(
            ConfigOverrides {
                role: Some(Role::Worker),
                worker_server: Some("http://coordinator".into()),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                control_seal_key_file: Some(PathBuf::from("/run/secrets/control-seal-key")),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(key_error.contains("control_seal_key_file"));

        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides {
                role: Some(Role::Worker),
                worker_server: Some("http://coordinator".into()),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                credential_db: Some("postgres://authority/credential".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("credential_db"));
    }

    #[test]
    fn control_rejects_managed_execution_database_bindings() {
        // Cause/effect decision table:
        // C1 Control without runtime/Session bindings -> accepted.
        // C2 any such binding -> every offending field is rejected before store
        // assembly. Shared management/resource bindings remain valid because
        // Control owns authoring stores and the resource inventory. C3 Coordinator
        // and C4 AllInOne own/compose execution and may receive the same fields.
        assert!(
            enforce_control_execution_database_isolation(Role::Control, &[]).is_ok(),
            "C1"
        );
        let error = enforce_control_execution_database_isolation(
            Role::Control,
            &[("runtime_database_url", true), ("sessions_db", true)],
        )
        .unwrap_err();
        assert!(error.contains("runtime_database_url"), "C2");
        assert!(error.contains("sessions_db"), "C2");
        assert!(
            enforce_control_execution_database_isolation(
                Role::Coordinator,
                &[("runtime_database_url", true)],
            )
            .is_ok(),
            "C3"
        );
        assert!(
            enforce_control_execution_database_isolation(Role::AllInOne, &[("sessions_db", true)],)
                .is_ok(),
            "C4"
        );
    }

    #[test]
    fn worker_transport_credentials_follow_process_ownership() {
        // Cause/effect decision table:
        // R1 Worker + Coordinator enrollment file -> reject cross-owner config.
        // R2 non-Worker + Worker signing file -> reject secret custody leak.
        // R3 Worker + Worker signing file -> accept the projected boundary path.
        // R4 AllInOne + enrollment file -> accept the trust boundary path.
        let resolve = |role, file| {
            ResolvedDeployment::resolve_file(
                ConfigOverrides {
                    role: Some(role),
                    worker_server: (role == Role::Worker).then(|| "http://coordinator".to_owned()),
                    ..Default::default()
                },
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                file,
            )
        };
        assert!(
            resolve(
                Role::Worker,
                FileConfig {
                    worker_trust_credentials_file: Some("/trust.json".into()),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .contains("owned by Coordinator"),
            "R1"
        );
        assert!(
            resolve(
                Role::AllInOne,
                FileConfig {
                    worker_request_credential_file: Some("/worker.json".into()),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .contains("owned by Worker"),
            "R2"
        );
        assert!(
            resolve(
                Role::Worker,
                FileConfig {
                    worker_request_credential_file: Some("/worker.json".into()),
                    ..Default::default()
                }
            )
            .is_ok(),
            "R3"
        );
        assert!(
            resolve(
                Role::AllInOne,
                FileConfig {
                    worker_trust_credentials_file: Some("/trust.json".into()),
                    ..Default::default()
                }
            )
            .is_ok(),
            "R4"
        );
    }
}
