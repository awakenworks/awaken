// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The canonical durable MCP aggregate decides whether one generation's realization lease
// @formal reason: currently holds one pinned bearer for a Worker holder; it adds no second MCP owner and
// @formal reason: moves no material. Classification, not a direct formal-proof claim.
//! The MCP bearer grant a Worker holds under one generation's realization lease,
//! and the refresh port that recovers its material after a challenge.

use std::sync::Arc;

use awaken_agent_contract::credential::{
    CredentialAccess, CredentialRef, CredentialRefresher, PlaintextBoundary,
};

use super::{McpAttachmentState, McpGenerationRef, SessionMcpAttachment, SessionMcpAttachmentSet};

/// Why a generation lease holds no such bearer grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCredentialGrantRefusal {
    /// The lease is not the generation's current, live realization claim.
    StaleGeneration,
    /// The current generation pins no such credential for a Worker holder.
    NotHeld,
}

impl SessionMcpAttachmentSet {
    /// The pinned credential a realization lease holds for one generation.
    ///
    /// | Rule | generation found, Realizing/Active | claim = lease (holder, epoch) | claim live | pin = credential | holder boundary | effect |
    /// |---|---|---|---|---|---|---|
    /// | G1 | no | any | any | any | any | StaleGeneration |
    /// | G2 | yes | no | any | any | any | StaleGeneration |
    /// | G3 | yes | yes | no | any | any | StaleGeneration |
    /// | G4 | yes | yes | yes | no | any | NotHeld |
    /// | G5 | yes | yes | yes | yes | not Worker | NotHeld |
    /// | G6 | yes | yes | yes | yes | Worker | the pinned access |
    pub fn credential_grant(
        &self,
        generation: &McpGenerationRef,
        credential: &CredentialRef,
        now_unix_ms: u64,
    ) -> Result<&CredentialAccess, McpCredentialGrantRefusal> {
        let attachment: &SessionMcpAttachment = self
            .attachments
            .iter()
            .find(|attachment| {
                &attachment.generation == generation.generation()
                    && matches!(
                        attachment.state,
                        McpAttachmentState::Realizing | McpAttachmentState::Active
                    )
            })
            .ok_or(McpCredentialGrantRefusal::StaleGeneration)?;
        let claim = attachment
            .realization
            .as_ref()
            .ok_or(McpCredentialGrantRefusal::StaleGeneration)?;
        if claim.runtime_incarnation != generation.runtime_incarnation()
            || claim.lease_epoch != generation.lease_epoch().get()
            || !crate::resource::realization_lease_is_live_at(
                claim.lease_expires_at_unix_ms,
                now_unix_ms,
            )
        {
            return Err(McpCredentialGrantRefusal::StaleGeneration);
        }
        match (
            &attachment.credential,
            &attachment.selected_plaintext_holder,
        ) {
            (Some(access), Some(holder))
                if &access.credential == credential
                    && holder.boundary == PlaintextBoundary::Worker =>
            {
                Ok(access)
            }
            _ => Err(McpCredentialGrantRefusal::NotHeld),
        }
    }
}

/// Recover the material of one MCP generation's pinned bearer after its server
/// answered with an authentication challenge. The in-process implementation
/// is the Vault refresh authority; a registered Worker's implementation asks
/// the Coordinator for the grant under the generation's realization lease.
pub trait McpCredentialRefresh: Send + Sync {
    fn refresher(
        &self,
        generation: &McpGenerationRef,
        credential: &CredentialAccess,
    ) -> Arc<dyn CredentialRefresher>;
}

#[cfg(test)]
mod tests {
    use super::super::{McpAttachmentDraft, McpAttachmentOrigin, McpRealizationClaim};
    use super::*;
    use awaken_agent_contract::McpTarget;
    use awaken_agent_contract::credential::{
        CredentialExecutionPolicy, CredentialMaterialSource, CredentialUsage, ModelExposurePolicy,
        PlaintextHolder,
    };

    fn pinned(id: &str, boundary: PlaintextBoundary) -> (CredentialAccess, PlaintextHolder) {
        let holder = PlaintextHolder::new(boundary, "awaken.worker");
        let access = CredentialAccess::new(
            CredentialRef {
                id: id.into(),
                revision: 3,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        );
        (access, holder)
    }

    fn claimed(boundary: PlaintextBoundary) -> (SessionMcpAttachmentSet, McpGenerationRef) {
        let (access, holder) = pinned("bearer", boundary);
        let mut set = SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "docs".into(),
                target: McpTarget::parse_http("https://docs.example.test").unwrap(),
                prompts_as_skills: false,
                credential: Some(access),
                origin: McpAttachmentOrigin::Session,
            }],
            Some(holder),
        )
        .unwrap();
        let generation = set.attachments[0].generation.clone();
        let claim = McpRealizationClaim {
            realization_id: "realization".into(),
            runtime_incarnation: "worker/boot".into(),
            lease_epoch: 4,
            lease_expires_at_unix_ms: 1_000,
            stage_idempotency_key: "stage".into(),
        };
        set.claim_realization(&generation, claim.clone()).unwrap();
        let reference = McpGenerationRef::from_claim(
            &awaken_session_types::SessionId::from("session"),
            &generation,
            &claim,
        )
        .unwrap();
        (set, reference)
    }

    #[test]
    fn a_generation_lease_holds_exactly_its_own_pinned_worker_bearer() {
        let credential = CredentialRef {
            id: "bearer".into(),
            revision: 3,
        };
        let (set, current) = claimed(PlaintextBoundary::Worker);
        // G6: the current, live claim holds its own Worker bearer.
        assert_eq!(
            set.credential_grant(&current, &credential, 500)
                .map(|access| access.credential.clone()),
            Ok(credential.clone()),
            "G6"
        );
        // G1: an unknown generation (another version of the attachment).
        let unknown = current
            .with_persisted(|wire| wire.generation = wire.generation.at_version(9))
            .unwrap();
        // G2: another incarnation, or another lease epoch of the same one.
        let foreign = current.reassigned("other/boot").unwrap();
        let older = current.with_persisted(|wire| wire.lease_epoch = 3).unwrap();
        for (rule, reference, now) in [
            ("G1", &unknown, 500),
            ("G2 holder", &foreign, 500),
            ("G2 epoch", &older, 500),
            ("G3", &current, 1_000),
        ] {
            assert_eq!(
                set.credential_grant(reference, &credential, now),
                Err(McpCredentialGrantRefusal::StaleGeneration),
                "{rule}"
            );
        }
        // G4: another credential, or another revision of the same one.
        for other in [
            CredentialRef {
                id: "other".into(),
                revision: 3,
            },
            CredentialRef {
                id: "bearer".into(),
                revision: 4,
            },
        ] {
            assert_eq!(
                set.credential_grant(&current, &other, 500),
                Err(McpCredentialGrantRefusal::NotHeld),
                "G4"
            );
        }
        // G5: a pin whose plaintext holder is the Platform is not a Worker grant.
        let (platform, platform_ref) = claimed(PlaintextBoundary::Platform);
        assert_eq!(
            platform.credential_grant(&platform_ref, &credential, 500),
            Err(McpCredentialGrantRefusal::NotHeld),
            "G5"
        );
    }
}
