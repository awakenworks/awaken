//! Every read over a poisoned installed-Session registry refuses with the one
//! typed poison class (`session_realization_lock_poisoned`) instead of
//! panicking or answering a default a caller could act on.
//!
//! Cause: a holder of the registry lock panicked. Effects, one row per reader:
//!
//! | Rule | reader                                         | answer |
//! |---|---|---|
//! | R1 | Workspace / Agent / model / credential profile | refused |
//! | R2 | capability advertisement (`capabilities_for`)  | refused, not the local Workspace's surface |
//! | R3 | live inbox snapshot / queue                    | `Unavailable`, not `Inactive` |
//! | R4 | mid-conversation system support               | refused, not a guessed capability |
//! | R5 | MCP root authority and effect authorization    | refused, never the lease-only fallback |
//! | R6 | MCP staging facts (backend, frozen network)    | refused, not absent |
//! | R7 | MCP application-input recipients               | refused, not an empty set |
//! | R8 | filesystem-tool policy, bound Memory           | refused |
//!
//! The healthy rows of each reader are owned by its component's tests.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use awaken_session_realization::realization_error::POISONED_CODE;
use awaken_session_types::RunError;

const SESSION: &str = "poisoned-session";

async fn poisoned_host() -> Arc<crate::ExecutorComponents> {
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("validated Host dependencies");
    let installed_sessions = &host.run_assembly.installed_sessions;
    installed_sessions
        .install(SESSION)
        .expect("healthy registry before poison");
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            installed_sessions.modify(SESSION, |_| -> () { panic!("poison the registry lock") })
        }))
        .is_err()
    );
    assert!(installed_sessions.installed_sessions.is_poisoned());
    host
}

fn generation() -> awaken_session_configuration_contract::mcp::McpGenerationRef {
    use awaken_session_configuration_contract::mcp::{
        McpAttachmentId, McpGeneration, McpGenerationRef,
    };
    McpGenerationRef::from_persisted(
        SESSION.into(),
        McpGeneration::of_bare_target(
            McpAttachmentId("phone".into()),
            1,
            &awaken_agent_contract::McpTarget::parse_http("https://phone.test/mcp").unwrap(),
        ),
        "worker-1".into(),
        1,
        u64::MAX,
    )
    .expect("nonzero epoch")
}

fn assert_poisoned(error: impl Into<RunError>, rule: &str) {
    let error = error.into();
    assert_eq!(error.code, POISONED_CODE, "{rule}: {error:?}");
}

#[tokio::test]
async fn poisoned_registry_refuses_every_session_read() {
    // Decision rows R1-R8 above share the same cause: the original canonical
    // registry is poisoned. Every reader must return its typed refusal before
    // any default Workspace, capability, generation or recipient is selected.
    use awaken_session_application_contract::LiveInboxError;
    use awaken_session_execution_contract::{
        CapabilityRuntime as _, LiveInboxAccess as _, StreamingRuns as _,
    };
    use awaken_session_mcp::{
        McpGenerationAuthority as _, McpInputHost as _, McpStagingSession as _,
    };

    let host = poisoned_host().await;
    let capabilities = &host.capabilities;
    let assembly = &host.run_assembly;

    assert_poisoned(
        capabilities
            .thread_workspace(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R1 workspace",
    );
    assert_poisoned(
        capabilities
            .thread_agent_projection(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R1 agent",
    );
    assert_poisoned(
        capabilities
            .registered_thread_workspace(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R1 registered workspace",
    );
    assert_poisoned(
        capabilities
            .model_for_thread(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R1 model",
    );
    // H1: the deleted Assembly forwarder read this exact canonical frozen profile;
    // its capability owner keeps R1 typed refusal without a new fallback/read law.
    assert_poisoned(
        capabilities
            .installed_sessions
            .realizations()
            .credential_realization(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R1 credential profile",
    );

    assert_poisoned(
        capabilities
            .capabilities_for(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R2",
    );

    assert_eq!(
        host.live_inboxes
            .live_inbox_snapshot(SESSION)
            .await
            .map(|_| ())
            .unwrap_err(),
        LiveInboxError::Unavailable,
        "R3 snapshot"
    );
    assert_eq!(
        host.live_inboxes
            .live_inbox_queue(SESSION, Vec::new())
            .await
            .map(|_| ())
            .unwrap_err(),
        LiveInboxError::Unavailable,
        "R3 queue"
    );

    assert_poisoned(
        host.session_runs
            .supports_mid_conversation_system(SESSION)
            .await
            .map(|_| ())
            .unwrap_err(),
        "R4",
    );

    let generation = generation();
    assert_poisoned(
        assembly
            .mcp_generation_root_authority(&generation)
            .map(|_| ())
            .unwrap_err(),
        "R5 root",
    );
    assert_poisoned(
        assembly
            .mcp_root_authority
            .mcp_generation_root_authority(&generation)
            .map(|_| ())
            .unwrap_err(),
        "R5 relay authority",
    );
    // The lease alone is live (expiry u64::MAX): an unreadable root must not
    // fall back to it.
    assert_poisoned(
        awaken_session_mcp::McpStagingSession::mcp_generation_is_authorized_at(
            assembly.as_ref(),
            &generation,
            1,
        )
        .map(|_| ())
        .unwrap_err(),
        "R5 effect authorization",
    );

    assert_poisoned(
        assembly.execution_backend(SESSION).map(|_| ()).unwrap_err(),
        "R6 backend",
    );
    assert_poisoned(
        assembly.frozen_network(SESSION).map(|_| ()).unwrap_err(),
        "R6 network",
    );

    assert_poisoned(
        assembly
            .mcp_application_inputs
            .active_mcp_projections(SESSION)
            .map(|_| ())
            .unwrap_err(),
        "R7",
    );

    assert_poisoned(
        assembly
            .session_allows_filesystem_tools(SESSION, None)
            .map(|_| ())
            .unwrap_err(),
        "R8 tool policy",
    );
    assert_poisoned(
        assembly.memory_for_thread(SESSION).map(|_| ()).unwrap_err(),
        "R8 Memory",
    );
}

#[tokio::test]
async fn poisoned_registry_refuses_session_run_projection_callbacks_before_fallback() {
    // Decision table P4/P5: the same original registry is poisoned; callback is
    // Workspace/Agent x known/unknown Thread. All four rows preserve the typed
    // poison code before local Workspace or Agent None can become an answer.
    // Factory construction reads nothing, even when the registry is unreadable.
    let host = poisoned_host().await;
    let (workspace, agent) = host.capabilities.session_run_projection_callbacks();
    for thread in [SESSION, "unknown-poisoned-projection"] {
        for error in [
            workspace(thread).map(|_| ()).unwrap_err(),
            agent(thread).map(|_| ()).unwrap_err(),
        ] {
            assert_eq!(error.kind, awaken_session_types::RunErrorKind::Unavailable);
            assert_poisoned(error, "P4/P5 original poison, no fallback");
        }
    }
}
