//! Attempt-wide filesystem interpretation derived from frozen model candidates.
//!
//! Prompt projection precedes fallback selection, so every eligible ACP
//! candidate must agree on its working directory. The selected Session
//! Environment later decides whether ACP and Native path consumers are
//! compatible with its provider-specific view.

#[derive(Debug, Eq, PartialEq)]
pub(super) struct AgentPromptFilesystemContext {
    pub(super) working_directory: Option<String>,
    pub(super) has_acp: bool,
    pub(super) has_native: bool,
}

pub(super) fn consistent_agent_filesystem_context(
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) -> awaken_runtime_contract::execution::Result<AgentPromptFilesystemContext> {
    use awaken_runtime_contract::resolved::{Backend, ModelProvisioning};

    let mut selected_acp_working_directory: Option<Option<String>> = None;
    let mut has_acp = false;
    let mut has_native = false;
    for candidate in snapshot.resolved_spec.execution_candidates(None) {
        let working_directory = match candidate.provisioning() {
            ModelProvisioning::BackendOwned { acp, .. } => {
                acp.session_configuration.working_directory.clone()
            }
            ModelProvisioning::Provider { acp, .. } => acp
                .as_deref()
                .and_then(|profile| profile.session_configuration.working_directory.clone()),
            ModelProvisioning::HostExecutor | ModelProvisioning::Remote { .. } => None,
        };
        if matches!(
            Backend::from_ref(candidate.binding().identity().2),
            Backend::Acp(_)
        ) {
            has_acp = true;
            match &selected_acp_working_directory {
                None => selected_acp_working_directory = Some(working_directory),
                Some(current) if current == &working_directory => {}
                Some(_) => {
                    return Err(awaken_runtime_contract::execution::Error::Resolution(
                        "all ACP model candidates must use the same working_directory".into(),
                    ));
                }
            }
        } else {
            has_native = true;
        }
    }
    if !has_acp && !has_native {
        return Err(awaken_runtime_contract::execution::Error::Resolution(
            "Agent has no executable model candidate".into(),
        ));
    }
    Ok(AgentPromptFilesystemContext {
        working_directory: selected_acp_working_directory.flatten(),
        has_acp,
        has_native,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::{
        AcpSessionConfiguration, BackendModelSelection, ModelBinding, ResolvedModelCandidate,
    };
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    fn backend_owned_candidate(
        model: &str,
        working_directory: Option<&str>,
    ) -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("local", model, "acp:codex"),
            awaken_runtime_contract::CredentialRef {
                id: "codex-login".into(),
                revision: 1,
            },
            BackendModelSelection::Exact,
            "codex-test",
            "sha256:codex-test",
            AcpSessionConfiguration {
                working_directory: working_directory.map(str::to_owned),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn model_pool_working_directory_is_one_attempt_invariant() {
        // Cause/effect graph: C1 candidate count={one,many}; C2 filesystem
        // context={same ACP cwd,different ACP cwd,Native+ACP clients}.
        // E1 one/equal produces one cwd and path interpretation for process,
        // ACP, prompt, and Hand tools; E2 differences fail before prompt or
        // process effects. Constraint: fallback can occur after context has
        // been injected, so every eligible candidate must share this setting.
        // Decision rules: W1 one=>E1; W2 many+equal=>E1;
        // W3 many+different-ACP-cwd=>E2; W4 native/default=>E1(Native);
        // W5 mixed Native/ACP=>retain both clients for provider-aware admission.
        let one = ExecutableAgentSnapshot::builder("agent")
            .resolved_model(backend_owned_candidate("one", Some("repo/src")))
            .build();
        assert_eq!(
            consistent_agent_filesystem_context(&one)
                .unwrap()
                .working_directory
                .as_deref(),
            Some("repo/src"),
            "W1"
        );
        let equal = ExecutableAgentSnapshot::builder("agent")
            .resolved_model(backend_owned_candidate("one", Some("repo/src")))
            .resolved_model_candidates([backend_owned_candidate("two", Some("repo/src"))])
            .build();
        assert_eq!(
            consistent_agent_filesystem_context(&equal)
                .unwrap()
                .working_directory
                .as_deref(),
            Some("repo/src"),
            "W2"
        );
        let different = ExecutableAgentSnapshot::builder("agent")
            .resolved_model(backend_owned_candidate("one", Some("repo/src")))
            .resolved_model_candidates([backend_owned_candidate("two", None)])
            .build();
        assert!(
            consistent_agent_filesystem_context(&different).is_err(),
            "W3"
        );
        let native = ExecutableAgentSnapshot::builder("agent")
            .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
                "provider", "native", "genai",
            )))
            .build();
        let native = consistent_agent_filesystem_context(&native).unwrap();
        assert!(native.has_native && !native.has_acp, "W4");
        let mixed = ExecutableAgentSnapshot::builder("agent")
            .resolved_model(backend_owned_candidate("one", None))
            .resolved_model_candidates([ResolvedModelCandidate::host(ModelBinding::new(
                "provider", "two", "genai",
            ))])
            .build();
        let mixed = consistent_agent_filesystem_context(&mixed).unwrap();
        assert!(mixed.has_native && mixed.has_acp, "W5");
    }
}
