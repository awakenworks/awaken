//! Shared complete MCP publication root for phase/receipt tests.
use super::*;

pub(crate) fn publication_fixture(
    id: &str,
) -> (
    PersistedSession,
    crate::session_resource::SessionRealizationLease,
    crate::session_mcp::McpGenerationRef,
) {
    let mut session = persisted(id, false, "activating");
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: (u64::MAX - 1_024),
    };
    let attachment_id = crate::session_mcp::McpAttachmentId("browser".into());
    let target = awaken_agent_contract::McpTarget::parse_http("https://browser.example.test/mcp")
        .expect("MCP target");
    let generation = crate::session_mcp::McpGeneration::new(
        attachment_id,
        1,
        crate::session_mcp::McpAttachmentContentDigest::of_content(
            &target,
            false,
            None,
            crate::session_mcp::McpAttachmentOrigin::Agent,
        ),
    );
    session.realization = Some(lease.clone());
    let claim = crate::session_mcp::McpRealizationClaim {
        realization_id: "realize-browser-1".into(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_epoch: lease.epoch,
        lease_expires_at_unix_ms: (u64::MAX - 1_024),
        stage_idempotency_key: "renew-browser-1".into(),
    };
    session.mcp.desired_names = Some(std::collections::BTreeSet::from(["browser".into()]));
    session
        .mcp
        .attachments
        .push(crate::session_mcp::SessionMcpAttachment {
            attachment_id: generation.attachment_id().clone(),
            name: "browser".into(),
            generation: generation.clone(),
            target,
            prompts_as_skills: false,
            origin: crate::session_mcp::McpAttachmentOrigin::Agent,
            credential: None,
            selected_plaintext_holder: None,
            state: crate::session_mcp::McpAttachmentState::Active,
            publication_acknowledged: false,
            realization: Some(claim.clone()),
            attempts: 1,
            last_error: None,
        });
    let generation_ref =
        crate::session_mcp::McpGenerationRef::from_claim(&id.into(), &generation, &claim);
    (session, lease, generation_ref.unwrap())
}
