//! Typed configuration for the private Control-to-Coordinator registration edge.

use std::path::{Path, PathBuf};

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

/// Role-aware inputs for the executable Agent registration adapters. The token
/// is projected as a file and loaded only by the process that owns the adapter;
/// it is never retained in the redacted deployment report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutableAgentRegistrationConfig {
    coordinator_url: Option<String>,
    token_file: Option<PathBuf>,
}

impl ExecutableAgentRegistrationConfig {
    pub(super) fn resolve(
        role: Role,
        coordinator_url: Option<String>,
        token_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        if role == Role::Worker && (coordinator_url.is_some() || token_file.is_some()) {
            return Err(
                "Worker must not receive executable Agent registration endpoint credentials".into(),
            );
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
            return Err("executable_agent_registration_token_file must not be empty".into());
        }
        if role != Role::Coordinator && coordinator_url.is_some() != token_file.is_some() {
            return Err(
                "coordinator_internal_url and executable_agent_registration_token_file must be configured together"
                    .into(),
            );
        }
        Ok(Self {
            coordinator_url,
            token_file,
        })
    }

    pub fn control_credentials(&self) -> Result<(&str, String), String> {
        let url = self.coordinator_url.as_deref().ok_or_else(|| {
            "Control requires coordinator_internal_url for executable Agent registration".to_owned()
        })?;
        Ok((url, load_token(self.token_file.as_deref())?))
    }

    pub fn coordinator_token(&self) -> Result<String, String> {
        load_token(self.token_file.as_deref())
    }
}

fn load_token(path: Option<&Path>) -> Result<String, String> {
    let path = path.ok_or_else(|| {
        "executable_agent_registration_token_file is required for split deployment".to_owned()
    })?;
    let token = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "read executable Agent registration token {}: {error}",
            path.display()
        )
    })?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(format!(
            "executable Agent registration token {} is empty",
            path.display()
        ));
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
        // Causes: missing file, empty file, and a file containing one token plus
        // a trailing newline. Effects: fail closed for the first two and return
        // only the trimmed token for the third; no environment fallback exists.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(load_token(Some(&missing)).is_err());
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "\n").unwrap();
        assert!(load_token(Some(&empty)).is_err());
        let valid = dir.path().join("valid");
        std::fs::write(&valid, "secret-token\n").unwrap();
        assert_eq!(load_token(Some(&valid)).unwrap(), "secret-token");
    }

    #[test]
    fn worker_rejects_every_authority_database_binding() {
        // Causes: Worker with no authority database versus one or several
        // Control/Coordinator/Resource database settings. Effects: the first is
        // accepted; every configured authority field is named in one fail-closed
        // error before a connection can be opened.
        assert!(enforce_worker_database_isolation(Role::Worker, &[]).is_ok());
        let error = enforce_worker_database_isolation(
            Role::Worker,
            &[("runtime_database_url", true), ("credential_db", true)],
        )
        .unwrap_err();
        assert!(error.contains("runtime_database_url"));
        assert!(error.contains("credential_db"));
        assert!(enforce_worker_database_isolation(Role::Control, &[("config_db", true)]).is_ok());

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
}
