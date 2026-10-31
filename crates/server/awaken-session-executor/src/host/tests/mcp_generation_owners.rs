//! T16-C02: hot replacement never lets one generation's cleanup consume
//! another generation's credential, route or process owner.

use super::*;
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
};
use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};

struct Process {
    id: String,
}

#[async_trait::async_trait]
impl pc::ProcessHandle for Process {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        Ok(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(Some(self.wait().await?))
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

const THREAD: &str = "mcp-owner-isolation";

async fn host_with_process_reaper() -> Arc<ExecutorComponents> {
    (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_mcp_process_reaper(Arc::new(awaken_run_executor_acp::Supervisor)))
    .build()
    .expect("validated Host dependencies")
}

fn generation(number: u64) -> McpGenerationRef {
    crate::test_mcp::mcp_fence(THREAD, "mcp-docs", number, "runtime-1", 1, u64::MAX)
}

/// One Active generation with its own bearer and its own process owner.
fn projection(number: u64, process: Arc<dyn pc::ProcessHandle>) -> McpGenerationProjection {
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation(number),
        realization_id: format!("realize-{number}"),
        stage_idempotency_key: format!("stage-{number}"),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(format!(
            "https://mcp-{number}.example.test"
        ))
        .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    McpGenerationProjection {
        receipt: McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: Some(awaken_session_mcp::McpTransportMaterial {
            name: "docs".into(),
            prompts_as_skills: false,
            transport: awaken_session_mcp::McpTransportMaterialKind::Http {
                url: format!("https://mcp-{number}.example.test"),
                bearer: Some(awaken_agent_contract::RedactedString::new(format!(
                    "secret-{number}"
                ))),
                refresh: None,
            },
        }),
        native_wiring: None,
        mcp_process: Some(process),
        platform_egress: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Active,
    }
}

/// The owner oracle: generation 1 is still Active with its own bearer and
/// exactly the process owner it was installed with.
fn first_generation_owned(
    host: &ExecutorComponents,
    process: &Arc<dyn pc::ProcessHandle>,
) -> Result<(), String> {
    let Some(first) = host.run_assembly.mcp_projection(&generation(1)) else {
        return Err("generation 1 projection disappeared".into());
    };
    if first.state != McpProjectionState::Active {
        return Err(format!("generation 1 left Active: {:?}", first.state));
    }
    if !first
        .mcp_process
        .as_ref()
        .is_some_and(|owner| Arc::ptr_eq(owner, process))
    {
        return Err("generation 1 lost its exact process owner".into());
    }
    let bearer = first
        .server
        .as_ref()
        .and_then(|server| server.bearer())
        .map(|bearer| bearer.expose_secret().to_owned());
    if bearer.as_deref() != Some("secret-1") {
        return Err(format!("generation 1 bearer changed: {bearer:?}"));
    }
    Ok(())
}

fn install_both(host: &ExecutorComponents) -> Arc<dyn pc::ProcessHandle> {
    let first: Arc<dyn pc::ProcessHandle> = Arc::new(Process {
        id: "process-1".into(),
    });
    let second: Arc<dyn pc::ProcessHandle> = Arc::new(Process {
        id: "process-2".into(),
    });
    host.run_assembly
        .insert_mcp_projection(projection(1, first.clone()))
        .unwrap();
    host.run_assembly
        .insert_mcp_projection(projection(2, second))
        .unwrap();
    first
}

/// | Rule | cleanup asserted under | Effect on generation 1 |
/// |---|---|---|
/// | O1 | exact generation 2 | untouched: Active, own bearer, same process Arc |
/// | O2 | generation 1 with a successor lease epoch | untouched |
/// | O3 | exact generation 1 | Removed |
/// | NC | Session-wide cleanup run when only generation 2 was asked | owner check fails |
#[tokio::test]
async fn generation_cleanup_never_consumes_another_generations_owner() {
    let host = host_with_process_reaper().await;
    let first_process = install_both(&host);

    host.run_assembly
        .drain_mcp_projection(&generation(2))
        .await
        .expect("O1 drain the replacement's exact owner");
    assert_eq!(
        host.run_assembly
            .mcp_projection(&generation(2))
            .expect("O1 tombstone")
            .state,
        McpProjectionState::Removed,
        "O1"
    );
    first_generation_owned(&host, &first_process).expect("O1");

    let successor = generation(1)
        .with_persisted(|fence| {
            fence.lease_epoch =
                awaken_lease_contract::fence::Epoch::from_persisted(fence.lease_epoch)
                    .and_then(awaken_lease_contract::fence::Epoch::next)
                    .expect("a persisted epoch has a successor")
                    .get();
        })
        .unwrap();
    host.run_assembly
        .drain_mcp_projection(&successor)
        .await
        .expect("O2 an unknown tenure is a no-op");
    first_generation_owned(&host, &first_process).expect("O2");

    host.run_assembly
        .drain_mcp_projection(&generation(1))
        .await
        .expect("O3 exact owner drains");
    assert_eq!(
        host.run_assembly
            .mcp_projection(&generation(1))
            .expect("O3 tombstone")
            .state,
        McpProjectionState::Removed,
        "O3"
    );

    // NC: an implementation that cleans "the Session" when asked for one
    // generation. The production Session-wide quiescence call stands in for
    // that mutant; the owner oracle must reject its effect.
    let mutant = host_with_process_reaper().await;
    let mutant_first = install_both(&mutant);
    let _ = mutant
        .run_assembly
        .drain_mcp_projections(THREAD, &[generation(2)])
        .await;
    let violation = first_generation_owned(&mutant, &mutant_first)
        .expect_err("NC Session-wide cleanup consumes generation 1");
    assert!(violation.contains("generation 1"), "NC {violation}");
}
