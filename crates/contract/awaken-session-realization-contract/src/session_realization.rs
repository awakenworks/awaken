// @formal entrypoint session.missing_publication_is_available_only_to_noninteractive_frozen_projection
// @formal entrypoint session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal evidence session_realization.publication_excludes_staged_retired_generation
//! Control-owned Session realization phase protocol (ADR-0066 D5).
//!
//! The Session aggregate owns durable desired and lifecycle state. A Runtime
//! projection owns only staged/live effects. These commands let local and remote
//! topology adapters drive the same root-CAS transitions without giving a Worker
//! repository access or creating a second MCP desired-state registry.

use async_trait::async_trait;

use crate::{
    McpGenerationRef, McpRealizationReceipt, RunError, SessionRealizationLease, StageMcpAttachment,
};

mod agent_publication_projection;
pub use agent_publication_projection::{
    FrozenAgentPublicationDecision, frozen_agent_publication_decision,
    frozen_sandbox_layout_publication_decision, legacy_agent_publication_decision,
    project_effective_agent_publication,
};
mod repository_publication_projection;
pub use repository_publication_projection::SessionRepositoryPublicationProjection;
mod control_failure;
mod exclusion;
pub use exclusion::{
    SessionRealizationExclusionDecision, SessionRealizationPermit,
    session_realization_exclusion_admits,
};
mod generation_authority;
pub use generation_authority::realization_generation_authorizes;
mod lease_receipt;
pub use control_failure::{SessionRealizationControlDisposition, SessionRealizationControlFailure};
pub use lease_receipt::{ObservedSessionRealizationLease, SessionRealizationLeaseReceipt};
mod terminal_cleanup_authorization;
pub use terminal_cleanup_authorization::{
    SessionTerminalCleanupAssignment, SessionTerminalCleanupPreparationAuthorization,
    SessionTerminalCleanupWork,
};

pub use awaken_session_resource_contract::{
    realization_lease_authorizes, realization_lease_generation_authorizes,
    realization_lease_is_live_at,
};

#[cfg(test)]
mod realization_assignment_tests {
    use super::*;

    #[test]
    fn assignment_follows_the_owner_incarnation_epoch_decision_table() {
        // Cause/effect graph: C1 a current fence exists and is live; C2 owner
        // matches; C3 Runtime incarnation matches; C4 authenticated topology
        // explicitly requests reassignment; C5 epoch can advance.
        // Effects: E1 reuse only the exact live owner/incarnation; E2 reject a
        // live foreign owner; E3 every new assignment mints exactly successor
        // epoch (or 1 from absence); E4 exhaustion fails closed.
        //
        // | Rule | Live | Owner | Incarnation | Reassign | Effect |
        // |---|---|---|---|---|---|
        // | R1 | yes | same | same | no | E1 reuse |
        // | R2 | yes | other | any | no | E2 stale |
        // | R3 | no | any | any | any | E3 successor |
        // | R4 | yes | same | other | no | E3 successor |
        // | R5 | yes | other | any | yes | E3 successor |
        // | R6 | assignment | any | any | any | E4 exhausted |
        // | R7 | yes | same | same | yes | E3 successor |
        // R7 models an approval continuation after normal Work retirement in
        // the SAME Worker process. Its local predecessor epoch may already be
        // closed, so authenticated reassignment cannot reuse that epoch. This
        // differs from R1 ordinary application replay without a claim grant.
        assert_eq!(
            session_realization_assignment(Some(7), true, true, true, false),
            SessionRealizationAssignment::Reuse,
            "R1/E1"
        );
        assert_eq!(
            session_realization_assignment(Some(7), true, false, false, false),
            SessionRealizationAssignment::StaleOwnership,
            "R2/E2"
        );
        assert_eq!(
            session_realization_assignment(Some(7), false, false, false, false),
            SessionRealizationAssignment::Assign { epoch: 8 },
            "R3/E3"
        );
        assert_eq!(
            session_realization_assignment(Some(7), true, true, false, false),
            SessionRealizationAssignment::Assign { epoch: 8 },
            "R4/E3"
        );
        assert_eq!(
            session_realization_assignment(Some(7), true, false, false, true),
            SessionRealizationAssignment::Assign { epoch: 8 },
            "R5/E3"
        );
        assert_eq!(
            session_realization_assignment(Some(u64::MAX), false, true, true, false),
            SessionRealizationAssignment::EpochExhausted,
            "R6/E4"
        );
        for epoch in [0, 7, u64::MAX - 1, u64::MAX] {
            assert_eq!(
                session_realization_assignment(Some(epoch), true, true, true, true),
                epoch
                    .checked_add(1)
                    .map_or(SessionRealizationAssignment::EpochExhausted, |epoch| {
                        SessionRealizationAssignment::Assign { epoch }
                    },),
                "R7/E3; R6/E4 at exhaustion"
            );
        }
    }
}

#[cfg(test)]
mod repository_publication_projection_tests {
    use super::{SessionRealizationLease, SessionRepositoryPublicationProjection};

    fn command() -> crate::SessionRepositoryPublicationCommand {
        crate::SessionRepositoryPublicationCommand {
            session_id: "publication-session".into(),
            effect_id: "publication-effect".into(),
            intent: serde_json::from_value(serde_json::json!({
                "input": {
                    "binding_id": "repository-binding",
                    "source": {
                        "kind": "repository",
                        "repository_id": "repository",
                        "config": {
                            "repository_id": "repository",
                            "version": 1,
                            "remote_url": "https://git.invalid/repository.git",
                            "initial_branch": "main"
                        }
                    },
                    "mount_path": "/workspace/repository",
                    "access": "read_write"
                },
                "expectation": {
                    "branch": "awf/publication",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                }
            }))
            .expect("valid publication intent"),
        }
    }

    #[test]
    fn repository_publication_projection_wire_is_closed_and_lease_canonical() {
        // Cause/effect graph: C1 the projection has an immutable Workspace and
        // command; C2 current_lease has a complete canonical generation and a
        // nonzero expiry; C3 either the outer projection or nested lease carries
        // an unknown/missing field. Effects: E1 exact values round-trip; E2
        // unknown or missing authority is rejected; E3 structurally unusable
        // lease identity/expiry is rejected before reaching a transport.
        //
        // | Rule | command/Workspace | current lease | wire | Effect |
        // |---|---|---|---|---|
        // | R1 | exact | canonical | closed | E1 |
        // | R2 | exact | canonical | outer/nested unknown | E2 |
        // | R3 | exact | missing | closed | E2 |
        // | R4 | exact | blank owner or zero expiry | closed | E3 |
        let projection = SessionRepositoryPublicationProjection::try_new(
            "workspace".into(),
            command(),
            SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/boot".into(),
                epoch: 7,
                expires_at_unix_ms: 20,
            },
        )
        .expect("R1/E1 canonical projection");
        let encoded = serde_json::to_value(&projection).expect("R1/E1 encode");
        assert_eq!(
            serde_json::from_value::<SessionRepositoryPublicationProjection>(encoded.clone())
                .expect("R1/E1 decode"),
            projection,
            "R1/E1"
        );

        for (rule, mut invalid) in [
            ("R2 outer", encoded.clone()),
            ("R2 nested", encoded.clone()),
        ] {
            if rule == "R2 outer" {
                invalid["parallel_authority"] = serde_json::json!(true);
            } else {
                invalid["current_lease"]["parallel_owner"] = serde_json::json!("other");
            }
            assert!(
                serde_json::from_value::<SessionRepositoryPublicationProjection>(invalid).is_err(),
                "{rule}/E2"
            );
        }

        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .expect("R3 object")
            .remove("current_lease");
        assert!(
            serde_json::from_value::<SessionRepositoryPublicationProjection>(missing).is_err(),
            "R3/E2"
        );

        for (rule, field, value) in [
            ("R4 owner", "owner", serde_json::json!(" ")),
            ("R4 expiry", "expires_at_unix_ms", serde_json::json!(0)),
        ] {
            let mut invalid = encoded.clone();
            invalid["current_lease"][field] = value;
            assert!(
                serde_json::from_value::<SessionRepositoryPublicationProjection>(invalid).is_err(),
                "{rule}/E3"
            );
        }
    }
}

/// Driving port for the Control-owned realization state machine. It performs no
/// Runtime I/O; topology adapters execute the returned action and submit exact
/// receipts/acknowledgements to the next phase.
#[async_trait]
pub trait SessionRealizationControl: Send + Sync {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    /// Renew only the existing realization fence. Initial assignment and
    /// desired-state progression remain owned by `begin`; heartbeat callers
    /// must not rebuild or reinstall an unchanged frozen projection merely to
    /// prove continued ownership.
    async fn renew_session_realization(
        &self,
        command: RenewSessionRealization,
    ) -> Result<SessionRealizationLeaseReceipt, SessionRealizationControlFailure>;

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure>;
}

/// Re-derive the Workspace for one exact pending Resource target under the
/// current realization lease. This authorizes only Resource staging; it is
/// neither a Run claim nor a second realization queue.
#[async_trait]
pub trait SessionResourceMaterializationControl: Send + Sync {
    async fn authorize_resource_materialization_intent(
        &self,
        intent: &SessionResourceMaterializationIntent,
    ) -> Result<String, SessionRealizationControlFailure>;
}

/// Authorization for checkpoint source-release Artifact publication.
#[async_trait]
pub trait SessionCheckpointArtifactControl: Send + Sync {
    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &crate::SessionId,
        operation: &crate::SessionEnvironmentOperation,
    ) -> Result<String, SessionRealizationControlFailure>;
}

/// Authorization for one aggregate-derived terminal Memory reconciliation.
#[async_trait]
pub trait SessionTerminalMemoryControl: Send + Sync {
    async fn authorize_terminal_memory_intent(
        &self,
        intent: &crate::SessionTerminalMemoryIntent,
    ) -> Result<crate::SessionTerminalMemoryTarget, SessionRealizationControlFailure>;
}

pub trait SessionResourceEffectControl:
    SessionResourceMaterializationControl
    + SessionCheckpointArtifactControl
    + SessionTerminalMemoryControl
{
}

impl<T> SessionResourceEffectControl for T where
    T: SessionResourceMaterializationControl
        + SessionCheckpointArtifactControl
        + SessionTerminalMemoryControl
        + ?Sized
{
}

/// Required cursor protocol for bounded terminal cleanup claim sweeps. An older
/// Worker that cannot advance a cursor is rejected before Control mutation.
pub const TERMINAL_CLEANUP_SWEEP_CURSOR_VERSION: u8 = 1;

/// Discovery and immutable work projection for aggregate-owned terminal cleanup.
#[async_trait]
pub trait SessionTerminalCleanupQueue: Send + Sync {
    /// Claim the next terminal Session after the exclusive, request-local
    /// Session cursor. A current incarnation may continue its own terminal
    /// cleanup; a replacement may recover an expired predecessor lease. The
    /// cursor only schedules one bounded sweep and grants no authority. The
    /// returned assignment contains only the
    /// facts needed to rebuild that process-local projection; callers must use
    /// [`Self::terminal_cleanup_work`] for the immutable cleanup commands and a
    /// current readback of the same frozen projection.
    /// Implementations that do not own external Worker placement return `None`.
    async fn claim_next_terminal_cleanup(
        &self,
        target: SessionRealizationTarget,
        after_session_id: Option<&crate::SessionId>,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure>;

    /// Project the exact terminal assignment and closed next action owned by this
    /// realization generation from one Session-root snapshot. `None` means the
    /// Session has no terminal fence or is durably complete; `Some(work)` with
    /// `Waiting` action means cleanup is fenced but not currently executable.
    /// Remote Workers poll this existing Control channel so the durable
    /// [`crate::SessionCleanupOperation`] remains the only queue.
    async fn terminal_cleanup_work(
        &self,
        session_id: &crate::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<Option<SessionTerminalCleanupWork>, SessionRealizationControlFailure>;
}

/// Preparation-effect authorization and exact receipt admission.
#[async_trait]
pub trait SessionTerminalPreparationControl: Send + Sync {
    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &crate::SessionTerminalCleanupEffect,
    ) -> Result<SessionTerminalCleanupPreparationAuthorization, SessionRealizationControlFailure>;

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &SessionRealizationLease,
        preparation: crate::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure>;
}

/// Aggregate-wide physical-disposal authorization and receipt admission.
#[async_trait]
pub trait SessionTerminalDisposalControl: Send + Sync {
    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &crate::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure>;

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &SessionRealizationLease,
        receipt: crate::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure>;
}

/// Exact root Repository publication projection and outcome admission.
#[async_trait]
pub trait SessionTerminalRepositoryPublicationControl: Send + Sync {
    async fn terminal_repository_publication_command(
        &self,
        session_id: &crate::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<Option<SessionRepositoryPublicationProjection>, SessionRealizationControlFailure>;

    /// Admit one exact root Repository publication receipt into the existing
    /// terminal cleanup operation. The explicit Session id is routing input,
    /// not duplicated receipt authority; implementations re-derive the command
    /// from that Session root before the root-CAS mutation.
    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &crate::SessionId,
        lease: &SessionRealizationLease,
        receipt: crate::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure>;

    /// Admit one exact permanent Repository publication rejection into the same
    /// cleanup sidecar and lease-fenced root CAS as a successful receipt. This is
    /// a terminal effect observation, not a Worker-local retry queue.
    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &crate::SessionId,
        lease: &SessionRealizationLease,
        rejection: crate::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure>;
}

/// Complete Worker terminal-cleanup role. The aggregate carries no additional
/// methods or state; it only states which exact roles the Worker composition
/// requires before it can start the recovery lane.
pub trait SessionTerminalCleanupControl:
    SessionTerminalCleanupQueue
    + SessionTerminalPreparationControl
    + SessionTerminalDisposalControl
    + SessionTerminalRepositoryPublicationControl
{
}

impl<T> SessionTerminalCleanupControl for T where
    T: SessionTerminalCleanupQueue
        + SessionTerminalPreparationControl
        + SessionTerminalDisposalControl
        + SessionTerminalRepositoryPublicationControl
        + ?Sized
{
}

/// Complete Control application surface. The canonical Session application
/// implements all three roles; narrower transports depend on only the role they
/// actually expose.
pub trait CompleteSessionRealizationControl:
    SessionRealizationControl + SessionResourceEffectControl + SessionTerminalCleanupControl
{
}

impl<T> CompleteSessionRealizationControl for T where
    T: SessionRealizationControl
        + SessionResourceEffectControl
        + SessionTerminalCleanupControl
        + ?Sized
{
}

/// Remote Worker realization surface: core lease/phase transitions plus the
/// terminal cleanup work that the Worker executes.
pub trait WorkerSessionRealizationControl:
    SessionRealizationControl + SessionTerminalCleanupControl
{
}

impl<T> WorkerSessionRealizationControl for T where
    T: SessionRealizationControl + SessionTerminalCleanupControl + ?Sized
{
}

/// Topology-specific projection installation used by the one realization
/// driver. Local Managed execution and a remote Worker both install the same
/// complete frozen projection through `SessionRuntime`; this port selects only
/// topology-specific effects and owns no lifecycle transition or desired state.
#[async_trait]
pub trait SessionProjectionSynchronizer: Send + Sync {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError>;

    async fn synchronize_session_projection(
        &self,
        session_id: &crate::SessionId,
        projection: &FrozenSessionProjection,
        lease: &ObservedSessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError>;

    /// Invalidate only the disposable projection of a failed exact lease. A
    /// retry must reload aggregate truth instead of reusing the fence that the
    /// failure command expired (or may have expired before response loss).
    async fn invalidate_session_realization_lease(
        &self,
        session_id: &crate::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<(), RunError>;
}

#[cfg(test)]
mod tests {
    use super::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        FailSessionRealization, RenewSessionRealization, SessionProjectionInstallMode,
        SessionRealizationControl, SessionRealizationControlFailure, SessionRealizationDirective,
        SessionTerminalCleanupPreparationAuthorization,
        SessionTerminalRepositoryPublicationControl, realization_lease_authorizes,
        realization_lease_is_live_at,
    };
    use crate::SessionRealizationLease;

    struct MinimalControl;

    #[tokio::test]
    async fn canonical_realization_renewal_port_is_exact_and_explicitly_refused_by_fixture() {
        // Cause/effect graph: C1 renewal names a Session and the complete
        // owner/incarnation/epoch lease; C2 it requests only a new expiry; C3 a
        // topology does or does not implement the aggregate renewal CAS.
        // Effects: E1 the canonical wire round-trips every exact fact; E2 the
        // explicit fixture refuses and cannot manufacture a terminal-specific
        // assignment, queue, or second renewal authority.
        //
        // | Rule | exact lease | expiry | implementation | Effect |
        // |---|---|---|---|---|
        // | R1 | present | present | any | E1 |
        // | R2 | present | present | absent | E2 |
        let command = RenewSessionRealization {
            session_id: "terminal-session".into(),
            asserted_lease: SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/boot".into(),
                epoch: 7,
                expires_at_unix_ms: 20,
            },
            requested_expires_at_unix_ms: 40,
        };
        let encoded = serde_json::to_value(&command).expect("R1/E1 encode");
        assert_eq!(
            serde_json::from_value::<RenewSessionRealization>(encoded)
                .expect("R1/E1 exact round trip"),
            command,
            "R1/E1"
        );
        assert!(
            matches!(
                MinimalControl.renew_session_realization(command).await,
                Err(SessionRealizationControlFailure::Invalid(_))
            ),
            "R2/E2"
        );
    }

    fn terminal_effect(
        thread_id: &str,
        epoch: u64,
        expires_at_unix_ms: u64,
    ) -> crate::SessionTerminalCleanupEffect {
        crate::SessionTerminalCleanupEffect::new(
            crate::SessionCleanupCommand::new(
                &crate::SessionId::from("session"),
                thread_id,
                "terminal-root",
            ),
            SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/boot".into(),
                epoch,
                expires_at_unix_ms,
            },
        )
    }

    #[test]
    fn terminal_preparation_authorization_is_exact_closed_and_root_only() {
        // Cause/effect graph: C1 the authorization echoes the exact effect; C2
        // its Workspace is non-empty; C3 inherited provider preparation is
        // absent, or belongs only to the root and precedes its realization
        // fence; C4 the wire contains only the closed schema. Effects: E1 exact
        // root/child values round-trip and verify; E2 cross-effect replay is
        // rejected; E3 child inheritance and non-successor inheritance are
        // rejected; E4 blank Workspace and unknown wire fields are rejected.
        //
        // | Rule | Effect binding | Workspace | inherited predecessor | wire | Effect |
        // | R1 | exact root | set | valid successor | closed | E1 |
        // | R2 | exact child | set | none | closed | E1 |
        // | R3 | foreign effect | set | any | closed | E2 |
        // | R4 | child | set | present | closed | E3 |
        // | R5 | root | set | later same-epoch fence | closed | E3 |
        // | R6 | root | blank | none | closed | E4 |
        // | R7 | exact | set | valid | unknown sibling | E4 |
        let root = terminal_effect("session", 4, 40);
        let child = terminal_effect("child", 4, 40);
        let inherited = awaken_provisioning_contract::SandboxDisposalPreparation::new(
            awaken_provisioning_contract::SandboxEffectFence::new(
                "continuation-preparation",
                "worker",
                "worker/boot",
                3,
                30,
            )
            .unwrap(),
            "continuation-fingerprint",
        )
        .unwrap();
        let root_authorization = SessionTerminalCleanupPreparationAuthorization::try_new(
            root.clone(),
            "workspace".into(),
            Some(inherited.clone()),
        )
        .expect("R1/E1");
        let encoded = serde_json::to_value(&root_authorization).unwrap();
        let decoded: SessionTerminalCleanupPreparationAuthorization =
            serde_json::from_value(encoded.clone()).expect("R1/E1 round trip");
        assert_eq!(decoded, root_authorization, "R1/E1 bytes");
        decoded.verify_for(&root).expect("R1/E1 exact effect");
        assert_eq!(decoded.workspace_id(), "workspace", "R1/E1 Workspace");
        assert_eq!(
            decoded.inherited_provider_disposal(),
            Some(&inherited),
            "R1/E1 predecessor"
        );

        let child_authorization = SessionTerminalCleanupPreparationAuthorization::try_new(
            child.clone(),
            "workspace".into(),
            None,
        )
        .expect("R2/E1");
        child_authorization.verify_for(&child).expect("R2/E1");
        assert!(child_authorization.inherited_provider_disposal().is_none());
        assert!(root_authorization.verify_for(&child).is_err(), "R3/E2");
        assert!(
            SessionTerminalCleanupPreparationAuthorization::try_new(
                child,
                "workspace".into(),
                Some(inherited),
            )
            .is_err(),
            "R4/E3"
        );

        let non_predecessor = awaken_provisioning_contract::SandboxDisposalPreparation::new(
            awaken_provisioning_contract::SandboxEffectFence::new(
                "later-preparation",
                "worker",
                "worker/boot",
                4,
                41,
            )
            .unwrap(),
            "later-fingerprint",
        )
        .unwrap();
        assert!(
            SessionTerminalCleanupPreparationAuthorization::try_new(
                root.clone(),
                "workspace".into(),
                Some(non_predecessor),
            )
            .is_err(),
            "R5/E3"
        );
        assert!(
            SessionTerminalCleanupPreparationAuthorization::try_new(root, " ".into(), None,)
                .is_err(),
            "R6/E4"
        );
        let mut unknown = encoded;
        unknown["parallel_authority"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<SessionTerminalCleanupPreparationAuthorization>(unknown)
                .is_err(),
            "R7/E4"
        );
    }

    fn legacy_resource_projection(bound: bool) -> super::FrozenSessionProjection {
        let baseline = crate::SessionBaseline::compile(crate::SessionBaselineInputs {
            environment: crate::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: crate::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: crate::EnvironmentFingerprint("environment-v1".into()),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: crate::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                    inference_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Workload,
                        "awaken.workload.acp",
                    ),
                    mcp_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Worker,
                        "awaken.worker",
                    ),
                    resource_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Worker,
                        "awaken.worker",
                    ),
                },
            },
            runtime_placement: crate::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        let mut environment = crate::SessionEnvironmentState::default();
        if bound {
            environment.set_resident("sandbox-binding");
        }
        super::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: crate::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment,
            resource_revision: 7,
            resources: Default::default(),
            previous_resource_manifest: None,
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    #[test]
    fn legacy_resource_transition_decision_table_is_fail_closed_for_effects() {
        // Cause/effect graph: C1 the legacy wire omits the aggregate-authored
        // previous manifest; C2 the durable Environment is bound or unbound;
        // C3 the caller either requires an effect-safe transition or proves it
        // will only validate an already-installed projection. Effects: E1 an
        // unbound Environment conservatively decodes empty->desired; E2 a bound
        // validation-only projection decodes desired->desired; E3 a bound
        // effect-capable projection fails closed. The typed use never changes
        // the unbound result.
        //
        // | Rule | C2 bound | C3 transition use | Effect |
        // |---|---|---|---|
        // | R1 | no | apply effects | E1 |
        // | R2 | no | validate installed | E1 |
        // | R3 | yes | apply effects | E3 |
        // | R4 | yes | validate installed | E2 |
        for (rule, bound, transition_use, expected_previous_revision) in [
            (
                "R1",
                false,
                super::FrozenResourceTransitionUse::ApplyEffects,
                Some(0),
            ),
            (
                "R2",
                false,
                super::FrozenResourceTransitionUse::ValidateInstalled,
                Some(0),
            ),
            (
                "R3",
                true,
                super::FrozenResourceTransitionUse::ApplyEffects,
                None,
            ),
            (
                "R4",
                true,
                super::FrozenResourceTransitionUse::ValidateInstalled,
                Some(7),
            ),
        ] {
            let projection = legacy_resource_projection(bound);
            let result = projection.resource_transition(transition_use);
            match expected_previous_revision {
                Some(expected) => {
                    let transition = result.expect(rule);
                    assert_eq!(transition.previous().revision, expected, "{rule}");
                    assert_eq!(transition.desired().revision, 7, "{rule}");
                }
                None => {
                    let error = result.expect_err(rule);
                    assert_eq!(error.code, "session_resource_transition_missing", "{rule}");
                }
            }
        }
    }

    #[test]
    fn projection_install_mode_effect_decision_table_is_closed() {
        // Cause/effect graph: C1 dispatch versus realization; C2 realization requests
        // preparation. Effects: E1 prepare execution context; E2 adopt a resident
        // Environment; E3 carry a realization lease. Decision rules:
        // D1 dispatch => E1,!E2,!E3; D2 realization+prepare => E1,E2,E3;
        // D3 realization+no-prepare => !E1,!E2,E3.
        let lease = SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 1,
            expires_at_unix_ms: 2,
        };
        let lease = super::SessionRealizationLeaseReceipt::issued_at(lease, 0)
            .unwrap()
            .observed(
                awaken_runtime_contract::authority_lease::AuthorityProofStarted::from_clock_sample(
                    awaken_lease_contract::mono::now(),
                    std::time::Duration::ZERO,
                ),
            );
        let rules = [
            (SessionProjectionInstallMode::Dispatch, true, false, false),
            (
                SessionProjectionInstallMode::Realization {
                    lease: lease.clone(),
                    prepare_session: true,
                },
                true,
                true,
                true,
            ),
            (
                SessionProjectionInstallMode::Realization {
                    lease,
                    prepare_session: false,
                },
                false,
                false,
                true,
            ),
        ];
        for (mode, prepares, adopts, has_lease) in rules {
            assert_eq!(mode.prepares_session(), prepares);
            assert_eq!(mode.adopts_resident_environment(), adopts);
            assert_eq!(mode.realization_lease().is_some(), has_lease);
        }
    }

    #[test]
    fn response_time_metadata_is_not_realization_phase_progress() {
        // C1 horizon changes; C2 exact lease changes; C3 phase changes;
        // C4 frozen projection changes. C1 alone is not progress; any of
        // C2/C3/C4 is. This guards against a phase loop kept alive solely
        // by fresh response timestamps while its business result repeats.
        let initial = super::SessionRealizationDirective {
            projection: legacy_resource_projection(false),
            authority: super::SessionRealizationLeaseReceipt::issued_at(
                SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "boot".into(),
                    epoch: 1,
                    expires_at_unix_ms: 100,
                },
                0,
            )
            .unwrap(),
            action: super::SessionRealizationAction::Stage {
                prepare_session: false,
                mcp_stages: Vec::new(),
            },
        };
        for changed in ["none", "time", "lease", "phase", "projection"] {
            let mut next = initial.clone();
            match changed {
                "none" => {}
                "time" => next.authority.remaining_ms = std::num::NonZeroU64::new(50).unwrap(),
                "lease" => next.authority.lease.expires_at_unix_ms += 1,
                "phase" => next.action = super::SessionRealizationAction::Complete,
                "projection" => next.projection.revision.0 += 1,
                _ => unreachable!(),
            }
            assert_eq!(
                next.same_progress_as(&initial),
                matches!(changed, "none" | "time"),
                "{changed}"
            );
        }
    }

    #[async_trait::async_trait]
    impl SessionRealizationControl for MinimalControl {
        async fn begin_session_realization(
            &self,
            _command: BeginSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn activate_session_realization(
            &self,
            _command: ActivateSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn acknowledge_session_realization(
            &self,
            _command: AcknowledgeSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn fail_session_realization(
            &self,
            _command: FailSessionRealization,
        ) -> Result<(), SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn renew_session_realization(
            &self,
            _command: RenewSessionRealization,
        ) -> Result<super::SessionRealizationLeaseReceipt, SessionRealizationControlFailure>
        {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no realization renewal authority".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl super::SessionTerminalCleanupQueue for MinimalControl {
        async fn claim_next_terminal_cleanup(
            &self,
            _target: super::SessionRealizationTarget,
            _after_session_id: Option<&crate::SessionId>,
        ) -> Result<Option<super::SessionTerminalCleanupAssignment>, SessionRealizationControlFailure>
        {
            Ok(None)
        }

        async fn terminal_cleanup_work(
            &self,
            _session_id: &crate::SessionId,
            _lease: &SessionRealizationLease,
        ) -> Result<Option<super::SessionTerminalCleanupWork>, SessionRealizationControlFailure>
        {
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl super::SessionTerminalPreparationControl for MinimalControl {
        async fn authorize_terminal_cleanup_effect(
            &self,
            _effect: &crate::SessionTerminalCleanupEffect,
        ) -> Result<SessionTerminalCleanupPreparationAuthorization, SessionRealizationControlFailure>
        {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no terminal cleanup effect authority".into(),
            ))
        }

        async fn record_terminal_cleanup_preparation(
            &self,
            _lease: &SessionRealizationLease,
            _preparation: crate::SessionCleanupPreparation,
        ) -> Result<(), SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no terminal cleanup receipt authority".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl super::SessionTerminalDisposalControl for MinimalControl {
        async fn authorize_terminal_cleanup_disposal(
            &self,
            _effect: &crate::SessionTerminalCleanupDisposalEffect,
        ) -> Result<String, SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no terminal cleanup disposal authority".into(),
            ))
        }

        async fn record_terminal_cleanup_disposal(
            &self,
            _lease: &SessionRealizationLease,
            _receipt: crate::SessionCleanupDisposalReceipt,
        ) -> Result<(), SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no terminal cleanup receipt authority".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl super::SessionTerminalRepositoryPublicationControl for MinimalControl {
        async fn terminal_repository_publication_command(
            &self,
            _session_id: &crate::SessionId,
            _lease: &SessionRealizationLease,
        ) -> Result<
            Option<super::SessionRepositoryPublicationProjection>,
            SessionRealizationControlFailure,
        > {
            Ok(None)
        }

        async fn record_terminal_repository_publication_receipt(
            &self,
            _session_id: &crate::SessionId,
            _lease: &SessionRealizationLease,
            _receipt: crate::SessionRepositoryPublicationReceipt,
        ) -> Result<(), SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no Repository publication authority".into(),
            ))
        }

        async fn record_terminal_repository_publication_rejection(
            &self,
            _session_id: &crate::SessionId,
            _lease: &SessionRealizationLease,
            _rejection: crate::SessionRepositoryPublicationRejection,
        ) -> Result<(), SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid(
                "fixture has no Repository publication authority".into(),
            ))
        }
    }

    #[tokio::test]
    async fn explicit_terminal_fixture_does_not_invent_work_or_receipts() {
        // Terminal-role cause/effect table: C1 an explicit test topology owns
        // the terminal role but has no Repository publication work; C2 it polls
        // for a command or submits a receipt. Effects: E1 polling returns None
        // from that explicit implementation; E2 receipt admission fails Invalid
        // and creates no synthetic root-CAS evidence. Rules D1=C1+poll=>E1;
        // D2=C1+record=>E2. Production absence is represented by not implementing
        // the role at all, never by inheriting these fixture decisions.
        let control = MinimalControl;
        let lease = SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 1,
            expires_at_unix_ms: 2,
        };
        assert!(
            control
                .terminal_repository_publication_command(&crate::SessionId::from("session"), &lease)
                .await
                .unwrap()
                .is_none(),
            "D1/E1"
        );
        let receipt: crate::SessionRepositoryPublicationReceipt =
            serde_json::from_value(serde_json::json!({
                "command_fingerprint": "command",
                "effect_receipt": {
                    "repository_id": "repo",
                    "source_remote_url": "https://example.test/repo.git",
                    "branch": "awf/work",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                },
                "receipt_fingerprint": "receipt"
            }))
            .unwrap();
        assert!(
            matches!(
                control
                    .record_terminal_repository_publication_receipt(
                        &crate::SessionId::from("session"),
                        &lease,
                        receipt,
                    )
                    .await,
                Err(SessionRealizationControlFailure::Invalid(_))
            ),
            "D2/E2"
        );
    }

    /// Cause C1: expiry is strictly after observation time. Only C1 authorizes
    /// another effect; equality is already outside the half-open lease.
    ///
    /// | Rule | expiry vs now | live |
    /// |---|---|---|
    /// | L1 | before | false |
    /// | L2 | equal | false |
    /// | L3 | after | true |
    #[test]
    fn realization_lease_boundary_decision_table() {
        for (expiry, now, expected, rule) in [
            (9, 10, false, "L1"),
            (10, 10, false, "L2"),
            (11, 10, true, "L3"),
        ] {
            assert_eq!(
                realization_lease_is_live_at(expiry, now),
                expected,
                "{rule}"
            );
        }
    }

    /// Realization-authorization cause/effect graph: C1 owner/incarnation/epoch
    /// identity is unchanged; C2 current expiry is equal to or later than the
    /// asserted expiry; C3 current lease is live. E1 authorizes the in-flight
    /// phase; otherwise E2 fences it. An asserted lease may itself have elapsed
    /// after admission because a live same-epoch extension owns continuation.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | A1 | yes | yes | yes | E1 |
    /// | A2 | no | any | yes | E2 |
    /// | A3 | yes | no | yes | E2 |
    /// | A4 | yes | yes | no | E2 |
    #[test]
    fn realization_lease_authorization_decision_table() {
        let asserted = SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 3,
            expires_at_unix_ms: 10,
        };
        for (rule, current, now, expected) in [
            ("A1 exact", asserted.clone(), 9, true),
            (
                "A1 renewed after asserted expiry",
                SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                11,
                true,
            ),
            (
                "A2 replaced owner",
                SessionRealizationLease {
                    owner: "worker-b".into(),
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                11,
                false,
            ),
            (
                "A3 regressed expiry",
                SessionRealizationLease {
                    expires_at_unix_ms: 9,
                    ..asserted.clone()
                },
                8,
                false,
            ),
            (
                "A4 current expired",
                SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                20,
                false,
            ),
        ] {
            assert_eq!(
                realization_lease_authorizes(&current, &asserted, now),
                expected,
                "{rule}"
            );
        }
    }
}

#[cfg(test)]
mod generation_authority_law_tests {
    use super::*;

    /// The delegated law equals the former field rule over every combination of
    /// lease-coordinate equality and expiry order, including the epoch-advance
    /// case the renewal branch must still reject.
    #[test]
    fn generation_authorization_is_exactly_the_former_field_rule() {
        let lease =
            |owner: &str, incarnation: &str, epoch: u64, expiry: u64| SessionRealizationLease {
                owner: owner.into(),
                runtime_incarnation: incarnation.into(),
                epoch,
                expires_at_unix_ms: expiry,
            };
        let asserted = lease("o", "i", 5, 100);
        for owner in ["o", "p"] {
            for incarnation in ["i", "j"] {
                for epoch in [4, 5, 6] {
                    for expiry in [99, 100, 101] {
                        let current = lease(owner, incarnation, epoch, expiry);
                        let former = current.owner == asserted.owner
                            && current.runtime_incarnation == asserted.runtime_incarnation
                            && current.epoch == asserted.epoch
                            && current.expires_at_unix_ms >= asserted.expires_at_unix_ms;
                        assert_eq!(
                            realization_lease_generation_authorizes(&current, &asserted),
                            former,
                            "owner={owner} incarnation={incarnation} epoch={epoch} expiry={expiry}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn lease_authorization_is_exactly_the_former_field_rule_with_liveness() {
        // The authorization law now runs through the one fence succession
        // kernel. Exhaust every axis the former inline rule compared and
        // assert the outcomes are identical: same owner, same incarnation,
        // same epoch, expiry never shortened, and still live at `now`.
        let lease =
            |owner: &str, incarnation: &str, epoch: u64, expiry: u64| SessionRealizationLease {
                owner: owner.into(),
                runtime_incarnation: incarnation.into(),
                epoch,
                expires_at_unix_ms: expiry,
            };
        let asserted = lease("o", "i", 5, 100);
        for owner in ["o", "p"] {
            for incarnation in ["i", "j"] {
                for epoch in [4, 5, 6] {
                    for expiry in [99, 100, 101] {
                        for now in [50, 99, 100, 101, 150] {
                            let current = lease(owner, incarnation, epoch, expiry);
                            let former = current.owner == asserted.owner
                                && current.runtime_incarnation == asserted.runtime_incarnation
                                && current.epoch == asserted.epoch
                                && current.expires_at_unix_ms >= asserted.expires_at_unix_ms
                                && current.expires_at_unix_ms > now;
                            assert_eq!(
                                realization_lease_authorizes(&current, &asserted, now),
                                former,
                                "owner={owner} incarnation={incarnation} epoch={epoch} expiry={expiry} now={now}"
                            );
                        }
                    }
                }
            }
        }
    }
}

mod model;
pub use model::*;
