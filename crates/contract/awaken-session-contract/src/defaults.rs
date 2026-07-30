//! Protocol-neutral compilation of Agent-bound defaults and per-Session input
//! overrides. Protocol adapters translate wire values into these inputs; none of
//! them may compose Resource bindings independently.

use awaken_resource_contract::{InputBinding, ResourceConfigSource};

use crate::{
    EnvironmentSnapshot, ResolvedSessionResources, SessionInputAttachment, SessionInputError,
    SessionInputResolver,
};

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledSessionDefaults {
    pub environment: EnvironmentSnapshot,
    pub resources: ResolvedSessionResources,
}

pub struct SessionDefaultsCompiler;

impl SessionDefaultsCompiler {
    /// Freeze the already-selected Environment together with the effective
    /// Resource bindings. Environment selection and authorization remain ports of
    /// the calling application service; Resource composition has exactly one
    /// implementation in [`SessionInputResolver`].
    pub fn compile(
        workspace_id: &str,
        catalog: Option<&dyn ResourceConfigSource>,
        environment: EnvironmentSnapshot,
        agent_defaults: &[InputBinding],
        session_attachments: &[SessionInputAttachment],
    ) -> Result<CompiledSessionDefaults, SessionInputError> {
        let resources = SessionInputResolver::resolve_inputs(
            workspace_id,
            catalog,
            agent_defaults,
            session_attachments,
        )?;
        Ok(CompiledSessionDefaults {
            environment,
            resources,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{BindingId, FileId, InputResourceId, ResourceAccess};

    fn environment(id: &str) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            environment_id: id.into(),
            revision: crate::env_registry::EnvironmentRevision(3),
            config_fingerprint: crate::EnvironmentFingerprint("env-fp".into()),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            packages: Default::default(),
            network: crate::SessionNetworkPolicy::None,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    fn binding(id: &str, file: &str, path: &str) -> InputBinding {
        InputBinding {
            binding_id: BindingId::from(id),
            target: InputResourceId::File(FileId::from(file)),
            mount_path: path.into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    #[test]
    fn decision_table_freezes_environment_and_uses_explicit_resource_replacement() {
        let default = binding("default", "file-a", "/input");
        let replacement = binding("replacement", "file-b", "/input");
        let compiled = SessionDefaultsCompiler::compile(
            "workspace",
            None,
            environment("env-a"),
            std::slice::from_ref(&default),
            &[SessionInputAttachment {
                binding: replacement,
                replaces: Some(default.binding_id.clone()),
            }],
        )
        .expect("D1 explicit replacement");
        assert_eq!(compiled.environment.environment_id, "env-a");
        assert_eq!(compiled.resources.inputs.len(), 1);
        assert_eq!(
            compiled.resources.inputs[0].binding_id.as_str(),
            "replacement"
        );
    }

    #[test]
    fn decision_table_rejects_implicit_collision() {
        let error = SessionDefaultsCompiler::compile(
            "workspace",
            None,
            environment("env-a"),
            &[binding("default", "file-a", "/input")],
            &[SessionInputAttachment {
                binding: binding("attachment", "file-b", "/input"),
                replaces: None,
            }],
        )
        .expect_err("D2 collision");
        assert!(matches!(error, SessionInputError::MountCollision(_)));
    }
}
