//! Persistence for the adapter-side Managed session aggregate.
//!
//! The runtime's committed transcript (ADR-0039) is durable, but the wire
//! `Session` object carries configuration that is NOT in the transcript — the
//! bound agent, the resolved model, the title/metadata, and the accepted MCP
//! servers. Without persisting it, a session rehydrated after a restart (or first
//! seen by another process sharing the store) reports placeholder defaults
//! (`agent = "assistant"`, empty `mcp_servers`, no title). This port stores that
//! aggregate so rehydration restores the real values.
//!
//! Secrets never cross this port. MCP and Repository entries may persist an
//! exact secret-free credential access/holder pin, but never credential material;
//! realization consumes that pin through the common exact resolver without
//! selecting another source or revision.

use std::collections::{BTreeMap, BTreeSet};

mod disposition;
mod persisted_session;
mod recovery;
mod repository_port;
mod repository_publication;
mod runtime_intervals;

use disposition::{SessionDeleteDispositionClass, session_delete_request_plan};
pub use disposition::{SessionDisposition, SessionDispositionTransitionError};
pub use persisted_session::PersistedSession;
pub use recovery::{
    SessionRecoveryCursor, SessionRecoveryQuarantine, SessionRecoveryScan,
    SessionRepositoryRecoveryAction,
};
pub use repository_port::{
    IdempotencyRecord, ManagedSessionRepository, ScopedPersistedSession, SessionCreateResult,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionMutationValidationError, SessionRealizationLease, SessionRepositoryConflict,
    SessionRepositoryError, SessionRevision, SessionTombstone, session_tombstone_is_admitted,
};
pub use repository_publication::SessionArchiveWithRepositoryPublicationError;

#[cfg(test)]
use crate::ManagedLifecycleFact;

mod execution_state;
pub use execution_state::{
    SessionExecutionState, SessionExecutionStateError, SessionExecutionTransitionError,
};

/// Secret-free active MCP projection consumed by protocol adapters. It is
/// derived from the typed attachment aggregate without a JSON serialization hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleMcpServer {
    pub name: String,
    pub target: crate::McpTarget,
    pub prompts_as_skills: bool,
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum PayloadKind {
        Replace,
        Delete,
    }

    #[derive(Clone)]
    struct Rule {
        id: &'static str,
        payload: PayloadKind,
        key_nonempty: bool,
        hash_nonempty: bool,
        session_id_nonempty: bool,
        revision_available: bool,
        payload_revision_exact: bool,
        lifecycle_session_exact: bool,
        expected: Result<SessionRevision, SessionMutationValidationError>,
    }

    pub(super) fn session(id: &str, revision: SessionRevision) -> PersistedSession {
        use awaken_credential_contract::{
            CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
        };

        PersistedSession {
            session_id: id.into(),
            revision,
            baseline: crate::SessionBaselineState::Preparing(crate::SessionCreationIntent {
                control: crate::ControlSessionCreationInputs {
                    mutation_policy: crate::SessionMutationPolicy::Managed,
                    environment: crate::EnvironmentSnapshot {
                        environment_id: "environment".into(),
                        revision: awaken_environment_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: crate::EnvironmentFingerprint("config".into()),
                        sandbox: Default::default(),
                        sandbox_provisioning: Default::default(),
                        idle_retention: Default::default(),
                        packages: Default::default(),
                        prepared_image: None,
                        network: crate::SessionNetworkPolicy::Unrestricted,
                        credential_realization: CredentialRealizationProfile {
                            inference_holder: PlaintextHolder::new(
                                PlaintextBoundary::Workload,
                                "awaken.workload.acp",
                            ),
                            mcp_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            resource_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                        },
                    },
                    runtime_placement: crate::SessionRuntimePlacement::Local,
                    agent_id: "assistant".into(),
                    agent_revision: None,
                    model: "model".into(),
                    execution_model_ref: "model".into(),
                    model_override: None,
                    system_prompt: crate::SessionSystemPromptSelection::Inherit,
                    runtime: None,
                    mcp_authoring: Default::default(),
                    toolsets: Vec::new(),
                    delegate_ids: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
            }),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: Default::default(),
            running_interval: None,
            closed_runtime_intervals: Vec::new(),
            runtime_active_millis: 0,
            usage_cursor: Default::default(),
            budget: Default::default(),
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            realization_progress: Default::default(),
            execution: SessionExecutionState::Idle,
            disposition: SessionDisposition::Active,
            terminal_cleanup: Default::default(),
        }
    }

    fn terminal_disposal_command(
        session: &PersistedSession,
    ) -> Option<crate::SessionCleanupDisposalCommand> {
        match session.terminal_cleanup_work_action().unwrap() {
            Some(crate::SessionTerminalCleanupAction::Dispose { command }) => Some(command),
            Some(crate::SessionTerminalCleanupAction::Waiting)
            | Some(crate::SessionTerminalCleanupAction::Prepare { .. })
            | None => None,
        }
    }

    #[test]
    fn frozen_constructor_owns_the_complete_initial_aggregate_shape() {
        // Cause/effect graph: C1 complete typed creation input is compiled; C2
        // no durable mutation has occurred. Effects: E1 every caller gets one
        // frozen baseline with Preparing execution, zero revision/activity,
        // complete effect aggregates, and exact presentation data; E2 no
        // protocol can construct a durable partially-authored root.
        //
        // | Rule | typed input | prior mutation | Effect |
        // | C1 | complete/frozen | no | E1 canonical aggregate |
        // | C2 | wire-specific defaults | no | E2 impossible at constructor |
        // Constraints/invariants: construction starts at revision/activity zero
        // with one complete frozen baseline; adapters cannot author partial roots.
        let fixture = session("constructor-source", SessionRevision(0));
        let crate::SessionBaselineState::Preparing(intent) = fixture.baseline else {
            panic!("fixture carries a creation intent");
        };
        let compiled = intent.finalize().expect("complete fixture compiles");
        let metadata = BTreeMap::from([("key".to_string(), "value".to_string())]);
        let prepared = PersistedSession::frozen_with_budget(
            "constructor",
            compiled.baseline,
            Default::default(),
            Default::default(),
            Some("title".into()),
            metadata.clone(),
            Default::default(),
            Default::default(),
        );
        assert_eq!(prepared.session_id, "constructor", "C1/E1");
        assert_eq!(prepared.revision, SessionRevision(0), "C1/E1");
        assert_eq!(
            prepared.execution,
            SessionExecutionState::Preparing,
            "C1/E1"
        );
        assert_eq!(prepared.disposition, SessionDisposition::Active, "C1/E1");
        assert_eq!(prepared.activity_epoch, 0, "C1/E1");
        assert!(prepared.active_activity_epochs.is_empty(), "C1/E1");
        assert!(prepared.frozen_baseline().is_some(), "C1/E1");
        assert_eq!(prepared.title.as_deref(), Some("title"), "C1/E1");
        assert_eq!(prepared.metadata, metadata, "C1/E1");
        assert!(prepared.mcp.attachments.is_empty(), "C1/E1");
        assert!(prepared.resources.active.inputs().is_empty(), "C1/E1");
        assert!(prepared.realization.is_none(), "C1/E1");
        assert!(prepared.terminal_cleanup.is_not_requested(), "C1/E1");
    }

    #[test]
    fn terminal_memory_authority_joins_cleanup_input_and_current_handle_exactly_once() {
        // Cause/effect decision table:
        // | Rule | cleanup | active input | current handle A | request | Effect |
        // |---|---|---|---|---|---|
        // | M1 | exact root pending | exact binding/store/config/path/RW | exact A | exact | admit |
        // | M2 | exact | same store, other mount/binding | exact A | other mount | resource mismatch |
        // | M3 | exact | exact | exact A | altered A | environment mismatch |
        // | M4 | exact | pending replacement exists | exact A | exact | resource mismatch |
        // | M5 | exact | exact | legacy/no evidence | exact | environment mismatch |
        // | M6 | child command | any | any | child | constructor rejects |
        // Constraints: caller evidence never becomes truth; only the serialized
        // current root binding supplies A. A store mounted twice is correlated by
        // binding id plus exact mount path, never by vector position or store id.
        let input = crate::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("memory-binding"),
            source: crate::ResolvedInputSource::MemoryStore {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-store"),
                config: awaken_resource_contract::MemoryStoreConfigVersion {
                    memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-store"),
                    version: awaken_resource_contract::ConfigVersion(4),
                    retention_policy: Default::default(),
                },
            },
            mount_path: "/memory/work".into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        };
        let resources =
            crate::ResolvedSessionResources::try_new(vec![input.clone()], Vec::new()).unwrap();
        let evidence = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "memory-store",
            "/memory/work",
            vec![awaken_provisioning_contract::MemoryMaterializationHead {
                path: "/note.md".into(),
                id: "memory-note".into(),
                content_sha256: "sha-a".into(),
            }],
        )
        .unwrap();
        let lease = crate::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker:1:boot".into(),
            epoch: 7,
            expires_at_unix_ms: 100,
        };
        let fingerprint = awaken_provisioning_contract::SandboxRealizationFingerprint::from_spec(
            &awaken_provisioning_contract::SandboxSpec {
                scope: "session-memory".into(),
                isolation: awaken_provisioning_contract::IsolationClass::Workdir,
                environment: None,
                command: Vec::new(),
                deny_tool_egress: false,
                mounts: Vec::new(),
                env: Vec::new(),
                packages: Default::default(),
                network: awaken_provisioning_contract::NetworkPolicy::Unrestricted,
                outputs_path: "/mnt/session/outputs".into(),
                requests: Default::default(),
                limits: Default::default(),
                filesystem_continuity: Default::default(),
                control_services: Default::default(),
                lease_ttl_secs: None,
            },
        );
        let handle = awaken_provisioning_contract::SandboxHandle::local_v2(
            "sandbox-memory",
            awaken_provisioning_contract::LocalSandboxHandleV2 {
                previous: awaken_provisioning_contract::LocalSandboxHandleV1 {
                    outputs_path: "/mnt/session/outputs".into(),
                    base_env: Vec::new(),
                    continuation_excluded_paths: Vec::new(),
                    deny_tool_egress: false,
                },
                realization_fingerprint: fingerprint,
                effect_fence: lease.sandbox_effect_fence("create-memory").unwrap(),
                physical_incarnation: "physical-memory".into(),
                owned_paths: vec!["/memory/work".into()],
            },
        )
        .with_memory_materializations(vec![evidence.clone()])
        .unwrap();

        let mut aggregate = session("session-memory", SessionRevision(1));
        aggregate.resources = crate::SessionResourceState::from_active(resources.clone());
        aggregate
            .environment
            .set_resident(serde_json::to_string(&handle).unwrap());
        aggregate.realization = Some(lease.clone());
        aggregate.ensure_terminal_cleanup_fence();
        aggregate.freeze_terminal_cleanup_targets([], 0, 0).unwrap();
        let command = aggregate
            .terminal_cleanup
            .pending_commands("session-memory")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let effect = crate::SessionTerminalCleanupEffect::new(command, lease);
        let exact =
            crate::terminal_memory_reconciliation_intent(&input, &evidence, &effect).unwrap();
        assert_eq!(
            aggregate.authorize_terminal_memory_intent(&exact),
            Ok(()),
            "M1"
        );

        let mut other_mount_input = input.clone();
        other_mount_input.binding_id = awaken_resource_contract::BindingId::from("memory-other");
        other_mount_input.mount_path = "/memory/other".into();
        let other_evidence = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "memory-store",
            "/memory/other",
            evidence.heads.clone(),
        )
        .unwrap();
        let other_mount = crate::terminal_memory_reconciliation_intent(
            &other_mount_input,
            &other_evidence,
            &effect,
        )
        .unwrap();
        assert_eq!(
            aggregate.authorize_terminal_memory_intent(&other_mount),
            Err(crate::SessionMemoryReconciliationError::ResourceMismatch),
            "M2"
        );

        let altered_evidence = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "memory-store",
            "/memory/work",
            vec![awaken_provisioning_contract::MemoryMaterializationHead {
                path: "/note.md".into(),
                id: "memory-note".into(),
                content_sha256: "sha-not-a".into(),
            }],
        )
        .unwrap();
        let altered =
            crate::terminal_memory_reconciliation_intent(&input, &altered_evidence, &effect)
                .unwrap();
        assert_eq!(
            aggregate.authorize_terminal_memory_intent(&altered),
            Err(crate::SessionMemoryReconciliationError::EnvironmentMismatch),
            "M3"
        );

        let mut pending = aggregate.clone();
        pending.resources.pending = Some(resources);
        assert_eq!(
            pending.authorize_terminal_memory_intent(&exact),
            Err(crate::SessionMemoryReconciliationError::ResourceMismatch),
            "M4"
        );

        let mut legacy = aggregate.clone();
        legacy.environment.set_resident(
            serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::local(
                "sandbox-memory",
                awaken_provisioning_contract::LocalSandboxHandleV1 {
                    outputs_path: "/mnt/session/outputs".into(),
                    base_env: Vec::new(),
                    continuation_excluded_paths: Vec::new(),
                    deny_tool_egress: false,
                },
            ))
            .unwrap(),
        );
        assert_eq!(
            legacy.authorize_terminal_memory_intent(&exact),
            Err(crate::SessionMemoryReconciliationError::EnvironmentMismatch),
            "M5"
        );

        let mut child_effect = effect;
        child_effect.command.thread_id = "child-thread".into();
        assert!(
            matches!(
                crate::terminal_memory_reconciliation_intent(&input, &evidence, &child_effect,),
                Err(crate::SessionMemoryReconciliationError::InvalidIntent(_))
            ),
            "M6"
        );
    }

    #[test]
    fn terminal_preparation_and_physical_disposal_retire_one_aggregate_atomically() {
        // Cause/effect graph: C1 the current realization lease is exact/stale;
        // C2 root preparation is absent/durable; C3 physical disposal is
        // absent/exact/foreign; C4 the final root-CAS response is delivered or
        // lost. Effects: E1 preparation may advance cleanup but cannot retire
        // Resource/Environment truth; E2 only Disposing authorizes physical
        // work; E3 exact disposal atomically retires those projections and
        // enters Completed; E4 exact response-loss replay is a no-op; E5 stale
        // or foreign evidence fails closed.
        //
        // | Rule | lease | preparation | disposal | Effect |
        // | A1 | exact | absent | any | no disposal authorization/E1 |
        // | A2 | stale | exact | absent | reject preparation/E5 |
        // | A3 | exact | exact | absent | Disposing, projections retained/E1/E2 |
        // | A3b | successor epoch | exact A | absent | disposer carries A→B/E2 |
        // | A4 | exact | exact | foreign | reject, projections retained/E5 |
        // | A5 | exact | exact | exact/replay | E3/E4 |
        let lease = crate::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a:boot".into(),
            epoch: 7,
            expires_at_unix_ms: u64::MAX,
        };
        let mut aggregate = session("prepared-root", SessionRevision(1));
        aggregate.resources = crate::SessionResourceState::from_active(
            crate::ResolvedSessionResources::try_new(
                vec![crate::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::from("prepared-memory"),
                    source: crate::ResolvedInputSource::MemoryStore {
                        memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                            "prepared-store",
                        ),
                        config: awaken_resource_contract::MemoryStoreConfigVersion {
                            memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                                "prepared-store",
                            ),
                            version: awaken_resource_contract::ConfigVersion(1),
                            retention_policy: Default::default(),
                        },
                    },
                    mount_path: "/memory/prepared".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap(),
        );
        aggregate.realization = Some(lease.clone());
        aggregate.environment.set_resident("sandbox-binding");
        assert!(aggregate.ensure_terminal_cleanup_fence());
        assert!(aggregate.freeze_terminal_cleanup_targets([], 3, 5).unwrap());
        assert!(terminal_disposal_command(&aggregate).is_none(), "A1/E1");
        let command = aggregate
            .terminal_cleanup
            .pending_preparation_commands("prepared-root")
            .unwrap()
            .pop()
            .unwrap();
        let preparation_effect = crate::SessionTerminalCleanupEffect::new(command, lease.clone());
        let preparation = crate::SessionCleanupPreparation::try_new(
            &preparation_effect,
            preparation_effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap();
        let repository_preparation = crate::SessionCleanupRepositoryPreparation::new(
            "prepared-root",
            "workspace",
            &aggregate.resources,
        )
        .unwrap();
        let mut stale = lease.clone();
        stale.epoch -= 1;
        assert_eq!(
            aggregate.record_terminal_cleanup_preparation(
                "workspace",
                &stale,
                preparation.clone(),
                Some(repository_preparation.clone()),
            ),
            Err(crate::SessionCleanupError::RealizationMismatch),
            "A2/E5"
        );
        assert!(
            aggregate
                .record_terminal_cleanup_preparation(
                    "workspace",
                    &lease,
                    preparation,
                    Some(repository_preparation),
                )
                .unwrap(),
            "A3"
        );
        assert!(aggregate.terminal_cleanup.is_requested(), "A3/E1");
        assert!(aggregate.environment.binding().is_some(), "A3/E1");
        assert!(!aggregate.resources.active.inputs().is_empty(), "A3/E1");

        let disposal = terminal_disposal_command(&aggregate).unwrap();
        let successor = crate::SessionRealizationLease {
            owner: "worker-b".into(),
            runtime_incarnation: "worker-b:boot".into(),
            epoch: lease.epoch + 1,
            expires_at_unix_ms: u64::MAX,
        };
        aggregate.realization = Some(successor.clone());
        let effect =
            crate::SessionTerminalCleanupDisposalEffect::new(disposal.clone(), successor.clone());
        assert_eq!(
            aggregate.authorize_terminal_cleanup_disposal_effect("workspace", &effect),
            Ok(()),
            "A3b/E2"
        );
        let provider_effect = effect.sandbox_disposal_authorization().expect("A3b/E2");
        assert_eq!(provider_effect.prepared_effect_fence().epoch, lease.epoch);
        assert_eq!(provider_effect.effect_fence().epoch, successor.epoch);
        let mut foreign = crate::SessionCleanupDisposalReceipt::new(&disposal);
        foreign.preparation_fingerprint.push_str("-foreign");
        assert_eq!(
            aggregate.record_terminal_cleanup_disposal(
                "workspace",
                &successor,
                foreign,
                "released",
            ),
            Err(crate::SessionCleanupError::DisposalReceiptMismatch),
            "A4/E5"
        );
        assert!(aggregate.environment.binding().is_some(), "A4/E5");

        let exact = crate::SessionCleanupDisposalReceipt::new(&disposal);
        assert!(
            aggregate
                .record_terminal_cleanup_disposal(
                    "workspace",
                    &successor,
                    exact.clone(),
                    "released",
                )
                .unwrap(),
            "A5/E3"
        );
        assert!(aggregate.terminal_cleanup.is_completed(), "A5/E3");
        assert!(aggregate.resources.active.inputs().is_empty(), "A5/E3");
        assert!(aggregate.environment.binding().is_none(), "A5/E3");
        aggregate.realization = None;
        assert!(
            !aggregate
                .record_terminal_cleanup_disposal("workspace", &successor, exact, "released",)
                .unwrap(),
            "A5/E4"
        );
    }

    #[test]
    fn restoring_rejects_target_free_legacy_completion_without_mutation() {
        // Cause/effect graph: C1 a complete historical one-stage receipt set
        // is present; C2 the Environment is Restoring or has no unpublished
        // restore target. Effects: E1 Restoring fails closed and retains both
        // exact target evidence and Requested cleanup; E2 a non-Restoring
        // historical row retains the existing decode-only normalization path.
        //
        // | Rule | complete legacy receipts | Environment | Effect |
        // |---|---|---|---|
        // | L1 | yes | Restoring | E1 ReceiptMismatch, no mutation |
        // | L2 | yes | Unmaterialized | E2 normalize to Completed |
        //
        // Constraint: legacy completion has no `SandboxRestoreRequest`, so it
        // can never prove disposal of a Phase-B unpublished physical target.
        let session_id = "legacy-restoring";
        let workspace_id = "workspace";
        let lease = crate::SessionRealizationLease {
            owner: "legacy-worker".into(),
            runtime_incarnation: "legacy-worker:incarnation".into(),
            epoch: 2,
            expires_at_unix_ms: u64::MAX,
        };
        let generation = crate::SandboxGeneration::new(
            session_id,
            1,
            90_000,
            "legacy-restoring-environment",
            "legacy-restoring-image",
        );
        let checkpoint = crate::SandboxCheckpointRef {
            id: "legacy-restoring-checkpoint".into(),
            format: "awaken-fs-tar-v1".into(),
            digest: "legacy-restoring-digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: 90_000,
            environment_fingerprint: generation.environment_fingerprint.clone(),
            base_image_fingerprint: generation.base_image_fingerprint.clone(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "legacy-suspend".into(),
        };
        let operation = crate::SessionEnvironmentOperation::new(
            workspace_id,
            session_id,
            "restore",
            &generation,
            1,
            Some(lease.clone()),
            Some(&checkpoint),
        );
        let mut aggregate = session(session_id, SessionRevision(1));
        aggregate.realization = Some(lease);
        aggregate.environment = crate::SessionEnvironmentState::Restoring {
            operation,
            checkpoint,
            generation,
        };
        assert!(aggregate.ensure_terminal_cleanup_fence());
        assert!(aggregate.freeze_terminal_cleanup_targets([], 0, 0).unwrap());
        let command = aggregate
            .terminal_cleanup
            .command_for(session_id, session_id)
            .expect("L1 legacy root command");
        let artifact_evidence = Vec::<(&str, &str)>::new();
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v1",
            command.session_id.as_str(),
            command.thread_id.as_str(),
            command.effect_id.as_str(),
            artifact_evidence.as_slice(),
        ));
        let mut wire = serde_json::to_value(&aggregate).unwrap();
        wire["terminal_cleanup"]["completions"] = serde_json::json!({
            (session_id): {
                "session_id": command.session_id,
                "thread_id": command.thread_id,
                "effect_id": command.effect_id,
                "artifact_receipts": [],
                "receipt_fingerprint": receipt_fingerprint,
            }
        });
        let mut recovered = serde_json::from_value::<PersistedSession>(wire)
            .expect("L1 exact historical aggregate decodes");
        assert_eq!(
            recovered.has_complete_legacy_terminal_cleanup_evidence(),
            Ok(true),
            "L1 complete historical evidence"
        );
        let before = recovered.clone();
        assert_eq!(
            recovered.normalize_legacy_terminal_cleanup("legacy released"),
            Err(crate::SessionCleanupError::ReceiptMismatch),
            "L1/E1"
        );
        assert_eq!(recovered, before, "L1/E1 atomic rejection");

        let mut ordinary = recovered;
        ordinary.environment = crate::SessionEnvironmentState::Unmaterialized;
        assert!(
            ordinary
                .normalize_legacy_terminal_cleanup("legacy released")
                .expect("L2 compatibility normalization"),
            "L2/E2"
        );
        assert!(ordinary.terminal_cleanup.is_completed(), "L2/E2");
    }

    #[test]
    fn terminal_provider_predecessor_survives_renewal_and_both_root_cas_orders() {
        // Provider-predecessor decision table TP1. Causes: C1 work assertion is
        // initial A or retry D; C2 the provider durably prepared C and returns C
        // on the D response-loss replay; C3 the A or D root CAS wins first; C4
        // the receipt reports exact C, a foreign operation/generation, or the
        // caller records it under a lease unequal to its work assertion.
        // Effects: E1 either winning CAS persists its own work assertion but
        // projects the same physical predecessor C; E2 the losing different
        // receipt is rejected without changing that predecessor; E3 foreign or
        // self-inconsistent evidence has zero aggregate mutation.
        //
        // | Rule | assertion | provider P | first CAS | Effect |
        // | TP1a | A | C | A | A+C durable, D rejected / E1-E2 |
        // | TP1b | D | C | D | D+C durable, A rejected / E1-E2 |
        // | TP1c | A/D | foreign | none | reject / E3 |
        // | TP1d | A | C | record under C | reject / E3 |
        let lease = |expires_at_unix_ms| crate::SessionRealizationLease {
            owner: "provider-worker".into(),
            runtime_incarnation: "provider-runtime".into(),
            epoch: 9,
            expires_at_unix_ms,
        };
        let lease_a = lease(10_000);
        let lease_c = lease(30_000);
        let lease_d = lease(40_000);
        let mut base = session("provider-race", SessionRevision(1));
        base.realization = Some(lease_d.clone());
        base.environment.set_resident("provider-binding");
        assert!(base.ensure_terminal_cleanup_fence());
        assert!(base.freeze_terminal_cleanup_targets([], 3, 5).unwrap());
        let command = base
            .terminal_cleanup
            .pending_preparation_commands("provider-race")
            .unwrap()
            .pop()
            .unwrap();
        let effect_a = crate::SessionTerminalCleanupEffect::new(command.clone(), lease_a.clone());
        let effect_d = crate::SessionTerminalCleanupEffect::new(command, lease_d.clone());
        let provider_c = lease_c
            .sandbox_effect_fence(effect_a.operation_id())
            .unwrap();
        let receipt_a =
            crate::SessionCleanupPreparation::try_new(&effect_a, provider_c.clone(), Vec::new())
                .unwrap();
        let receipt_d =
            crate::SessionCleanupPreparation::try_new(&effect_d, provider_c.clone(), Vec::new())
                .unwrap();
        let repository_preparation = crate::SessionCleanupRepositoryPreparation::new(
            "provider-race",
            "workspace",
            &base.resources,
        )
        .unwrap();

        for (rule, first_lease, first, second_lease, second) in [
            ("TP1a", &lease_a, &receipt_a, &lease_d, &receipt_d),
            ("TP1b", &lease_d, &receipt_d, &lease_a, &receipt_a),
        ] {
            let mut candidate = base.clone();
            assert!(
                candidate
                    .record_terminal_cleanup_preparation(
                        "workspace",
                        first_lease,
                        first.clone(),
                        Some(repository_preparation.clone()),
                    )
                    .unwrap(),
                "{rule}/E1"
            );
            assert_eq!(
                candidate.record_terminal_cleanup_preparation(
                    "workspace",
                    second_lease,
                    second.clone(),
                    Some(repository_preparation.clone()),
                ),
                Err(crate::SessionCleanupError::PreparationReceiptMismatch),
                "{rule}/E2",
            );
            assert_eq!(
                terminal_disposal_command(&candidate)
                    .unwrap()
                    .provider_disposal
                    .prepared_effect_fence(),
                &provider_c,
                "{rule}/E1 exact provider predecessor",
            );
        }

        let foreign = awaken_provisioning_contract::SandboxEffectFence::new(
            "foreign-operation",
            lease_a.owner.clone(),
            lease_a.runtime_incarnation.clone(),
            lease_a.epoch,
            lease_c.expires_at_unix_ms,
        )
        .unwrap();
        assert_eq!(
            crate::SessionCleanupPreparation::try_new(&effect_a, foreign, Vec::new()),
            Err(crate::SessionCleanupError::PreparationReceiptMismatch),
            "TP1c/E3",
        );
        let mut inconsistent = base.clone();
        assert_eq!(
            inconsistent.record_terminal_cleanup_preparation(
                "workspace",
                &lease_c,
                receipt_a,
                Some(repository_preparation),
            ),
            Err(crate::SessionCleanupError::RealizationMismatch),
            "TP1d/E3",
        );
        assert!(
            terminal_disposal_command(&inconsistent).is_none(),
            "TP1d/E3"
        );
    }

    #[test]
    fn continuation_provider_predecessor_survives_renewal_and_both_root_cas_orders() {
        // Continuation-predecessor decision table CP1. Causes: C1 source work
        // asserts original A or retry D; C2 provider preparation C completed
        // before its response was lost and the D retry returns immutable C; C3
        // A-CAS or D-CAS wins first; C4 P has exact or foreign operation/
        // generation. Effects: E1 either winner enters the one Disposing phase
        // with physical predecessor C; E2 the losing non-identical receipt is
        // rejected without changing C; E3 foreign P has zero aggregate mutation.
        //
        // | Rule | assertion | provider P | first CAS | Effect |
        // | CP1a | A | C | A | A+C durable, D rejected / E1-E2 |
        // | CP1b | D | C | D | D+C durable, A rejected / E1-E2 |
        // | CP1c | A/D | foreign | none | reject / E3 |
        let lease = |expires_at_unix_ms| crate::SessionRealizationLease {
            owner: "continuation-worker".into(),
            runtime_incarnation: "continuation-runtime".into(),
            epoch: 5,
            expires_at_unix_ms,
        };
        let lease_a = lease(10_000);
        let lease_c = lease(30_000);
        let lease_d = lease(40_000);
        let generation = crate::SandboxGeneration::new(
            "continuation-race",
            1,
            90_000,
            "continuation-env",
            "continuation-image",
        );
        let mut base = session("continuation-race", SessionRevision(1));
        base.realization = Some(lease_d.clone());
        base.environment = crate::SessionEnvironmentState::Resident {
            binding: "continuation-source".into(),
            effect_id: Some("create".into()),
            generation: Some(generation.clone()),
            idle_since_unix_ms: None,
        };
        let operation = base
            .environment
            .begin_suspend_at(
                "workspace",
                "continuation-race",
                base.activity_epoch,
                Some(lease_a.clone()),
                1_000,
            )
            .unwrap()
            .clone();
        base.environment
            .record_quiescence(
                &crate::QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation.id.clone(),
                    activity_epoch: base.activity_epoch,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        base.environment
            .record_checkpoint(&crate::CheckpointReceipt {
                effect_id: operation.effect_id.clone(),
                generation_id: generation.id.clone(),
                checkpoint: crate::SandboxCheckpointRef {
                    id: "continuation-checkpoint".into(),
                    format: "awaken-fs-v1".into(),
                    digest: "continuation-digest".into(),
                    size_bytes: 1,
                    created_at_unix_ms: 2_000,
                    expires_at_unix_ms: 80_000,
                    environment_fingerprint: generation.environment_fingerprint.clone(),
                    base_image_fingerprint: generation.base_image_fingerprint.clone(),
                    excluded_mounts: Vec::new(),
                    suspend_effect_id: operation.effect_id.clone(),
                },
            })
            .unwrap();
        let preparation_a =
            crate::SourceReleasePreparationEffect::new(operation.clone(), lease_a.clone()).unwrap();
        let preparation_d =
            crate::SourceReleasePreparationEffect::new(operation.clone(), lease_d.clone()).unwrap();
        let provider_c = lease_c
            .sandbox_effect_fence(operation.effect_id.as_str())
            .unwrap();
        let receipt_a = crate::SourceReleasePreparedReceipt::try_new(
            preparation_a.clone(),
            provider_c.clone(),
            &generation,
            "continuation-source",
        )
        .unwrap();
        let receipt_d = crate::SourceReleasePreparedReceipt::try_new(
            preparation_d.clone(),
            provider_c.clone(),
            &generation,
            "continuation-source",
        )
        .unwrap();

        for (rule, first, second) in [
            ("CP1a", &receipt_a, &receipt_d),
            ("CP1b", &receipt_d, &receipt_a),
        ] {
            let mut candidate = base.clone();
            assert!(
                candidate.record_source_release_prepared(first).unwrap(),
                "{rule}/E1"
            );
            assert_eq!(
                candidate.record_source_release_prepared(second),
                Err(crate::SessionEnvironmentReceiptError::Mismatch),
                "{rule}/E2",
            );
            assert_eq!(
                candidate
                    .source_release_disposal()
                    .unwrap()
                    .sandbox_disposal_authorization()
                    .unwrap()
                    .prepared_effect_fence(),
                &provider_c,
                "{rule}/E1 exact provider predecessor",
            );
        }

        let foreign = awaken_provisioning_contract::SandboxEffectFence::new(
            "foreign-continuation",
            lease_a.owner,
            lease_a.runtime_incarnation,
            lease_a.epoch,
            lease_c.expires_at_unix_ms,
        )
        .unwrap();
        assert_eq!(
            crate::SourceReleasePreparedReceipt::try_new(
                preparation_a,
                foreign,
                &generation,
                "continuation-source",
            ),
            Err(crate::SessionEnvironmentReceiptError::Mismatch),
            "CP1c/E3",
        );
        assert!(base.source_release_disposal().is_err(), "CP1c/E3");
    }

    #[test]
    fn terminal_disposal_inherits_the_exact_continuation_predecessor() {
        // Cause/effect graph: C1 the Environment is ordinary Resident or a
        // continuation whose source preparation A is durably Disposing; C2 the
        // terminal root preparation T is complete; C3 the current disposer is
        // the same generation, a higher epoch, or foreign. Effects: E1 ordinary
        // terminal cleanup derives provider predecessor T; E2 continuation
        // takeover derives A without copying it into terminal progress; E3 the
        // aggregate fingerprint remains terminal-owned; E4 only a canonical
        // A->successor authorization reaches the provider. Decision rules:
        //
        // | Rule | Environment | terminal prep | successor | Effect |
        // | X1 | Resident | T complete | live | provider T / E1 |
        // | X2 | Disposing(A) | T complete | higher epoch | provider A / E2-E4 |
        // | X3 | Disposing(A) | T incomplete | any | no disposal command |
        // | X4 | malformed/foreign A | T complete | any | fail before command |
        let source_lease = crate::SessionRealizationLease {
            owner: "source-worker".into(),
            runtime_incarnation: "source-runtime".into(),
            epoch: 4,
            expires_at_unix_ms: 40_000,
        };
        let generation = crate::SandboxGeneration::new(
            "takeover-session",
            3,
            30_000,
            "environment-fingerprint",
            "base-image-fingerprint",
        );
        let mut aggregate = session("takeover-session", SessionRevision(1));
        aggregate.realization = Some(source_lease.clone());
        aggregate.environment = crate::SessionEnvironmentState::Resident {
            binding: "source-binding".into(),
            effect_id: Some("create".into()),
            generation: Some(generation.clone()),
            idle_since_unix_ms: None,
        };
        let operation = aggregate
            .environment
            .begin_suspend_at(
                "workspace",
                "takeover-session",
                aggregate.activity_epoch,
                Some(source_lease.clone()),
                1_000,
            )
            .unwrap()
            .clone();
        aggregate
            .environment
            .record_quiescence(
                &crate::QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation.id.clone(),
                    activity_epoch: aggregate.activity_epoch,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        aggregate
            .environment
            .record_checkpoint(&crate::CheckpointReceipt {
                effect_id: operation.effect_id.clone(),
                generation_id: generation.id.clone(),
                checkpoint: crate::SandboxCheckpointRef {
                    id: "checkpoint".into(),
                    format: "awaken-fs-v1".into(),
                    digest: "digest".into(),
                    size_bytes: 42,
                    created_at_unix_ms: 2_000,
                    expires_at_unix_ms: 50_000,
                    environment_fingerprint: generation.environment_fingerprint.clone(),
                    base_image_fingerprint: generation.base_image_fingerprint.clone(),
                    excluded_mounts: Vec::new(),
                    suspend_effect_id: operation.effect_id.clone(),
                },
            })
            .unwrap();
        let source_preparation = aggregate.source_release_preparation_effect().unwrap();
        let source_prepared_effect_fence = source_preparation.sandbox_effect_fence().unwrap();
        let source_receipt = crate::SourceReleasePreparedReceipt::try_new(
            source_preparation,
            source_prepared_effect_fence,
            &generation,
            "source-binding",
        )
        .unwrap();
        assert!(
            aggregate
                .record_source_release_prepared(&source_receipt)
                .unwrap(),
            "X2 source preparation becomes durable"
        );

        let terminal_lease = crate::SessionRealizationLease {
            owner: "terminal-worker".into(),
            runtime_incarnation: "terminal-runtime".into(),
            epoch: source_lease.epoch + 1,
            expires_at_unix_ms: 60_000,
        };
        aggregate.disposition = SessionDisposition::Deleting;
        aggregate.realization = Some(terminal_lease.clone());
        assert!(aggregate.ensure_terminal_cleanup_fence());
        assert!(
            aggregate
                .freeze_terminal_cleanup_targets([], 7, 11)
                .unwrap()
        );
        assert!(terminal_disposal_command(&aggregate).is_none(), "X3");
        let root = aggregate
            .terminal_cleanup
            .pending_preparation_commands("takeover-session")
            .unwrap()
            .pop()
            .unwrap();
        let terminal_effect =
            crate::SessionTerminalCleanupEffect::new(root, terminal_lease.clone());
        assert!(
            aggregate
                .record_terminal_cleanup_preparation(
                    "workspace",
                    &terminal_lease,
                    crate::SessionCleanupPreparation::try_new(
                        &terminal_effect,
                        terminal_effect.sandbox_effect_fence().unwrap(),
                        Vec::new(),
                    )
                    .unwrap(),
                    Some(
                        crate::SessionCleanupRepositoryPreparation::new(
                            "takeover-session",
                            "workspace",
                            &aggregate.resources,
                        )
                        .unwrap(),
                    ),
                )
                .unwrap(),
            "X2 terminal preparation becomes durable"
        );
        let disposal = terminal_disposal_command(&aggregate).expect("X2/E2");
        let inherited = source_receipt.sandbox_disposal_preparation().unwrap();
        assert_eq!(disposal.provider_disposal, inherited, "X2/E2");
        assert_ne!(
            disposal
                .provider_disposal
                .prepared_effect_fence()
                .operation_id,
            terminal_effect.command.effect_id,
            "X2/E3 source and terminal preparation identities remain distinct"
        );
        let successor = crate::SessionRealizationLease {
            epoch: terminal_lease.epoch + 1,
            ..terminal_lease
        };
        let authorization = crate::SessionTerminalCleanupDisposalEffect::new(disposal, successor)
            .sandbox_disposal_authorization()
            .expect("X2/E4");
        assert_eq!(authorization.preparation(), inherited, "X2/E4");
    }

    #[test]
    fn persisted_session_accepts_only_the_complete_canonical_grammar() {
        // Grammar cause/effect partition: C1 complete canonical aggregate;
        // C2 unknown status; C3 missing pre-existing required fact; C4 missing
        // Event-batch truth; C5 missing active-activity truth; C6 old status
        // alias; C7 unknown top-level fact. E1 is exact decode and E2 is a
        // fail-closed decode error. Rules S1=C1=>E1 and S2..S7=C2..C7=>E2.
        // Recovery never synthesizes domain truth from defaults, SQL columns,
        // or alternate spellings.
        // Constraints/invariants: the persisted aggregate has one closed grammar;
        // missing, aliased, or unknown facts never receive compatibility defaults.
        let value = serde_json::to_value(session("session-1", SessionRevision(1))).unwrap();
        assert_eq!(value.get("status"), Some(&serde_json::json!("idle")));
        assert!(value.get("lifecycle").is_none());
        assert!(
            serde_json::from_value::<PersistedSession>(value.clone()).is_ok(),
            "S1"
        );

        let mut unknown = value.clone();
        unknown["status"] = serde_json::json!("legacy-unknown");
        assert!(
            serde_json::from_value::<PersistedSession>(unknown).is_err(),
            "S2"
        );

        let mut missing = value.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("realization_progress");
        assert!(
            serde_json::from_value::<PersistedSession>(missing).is_err(),
            "S3"
        );

        let mut missing_batches = value.clone();
        missing_batches
            .as_object_mut()
            .unwrap()
            .remove("event_batches");
        assert!(
            serde_json::from_value::<PersistedSession>(missing_batches).is_err(),
            "S4"
        );

        let mut missing_activities = value.clone();
        missing_activities
            .as_object_mut()
            .unwrap()
            .remove("active_activity_epochs");
        assert!(
            serde_json::from_value::<PersistedSession>(missing_activities).is_err(),
            "S5"
        );

        let mut aliased = value.clone();
        let status = aliased.as_object_mut().unwrap().remove("status").unwrap();
        aliased["lifecycle"] = status;
        assert!(
            serde_json::from_value::<PersistedSession>(aliased).is_err(),
            "S6"
        );

        let mut extra = value;
        extra["unknown"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<PersistedSession>(extra).is_err(),
            "S7"
        );
    }

    #[test]
    fn repository_failure_policy_is_closed_and_exhaustive() {
        use SessionRepositoryError as Error;
        use SessionRepositoryRecoveryAction as Action;

        assert_eq!(
            Error::Unavailable("db offline".into()).recovery_action(),
            Action::Retry
        );
        assert_eq!(
            Error::Corrupt("negative revision".into()).recovery_action(),
            Action::Quarantine
        );
        assert_eq!(Error::NotFound.recovery_action(), Action::Reject);
        assert_eq!(
            Error::Conflict(SessionRepositoryConflict::AlreadyExists).recovery_action(),
            Action::Reject
        );
        assert_eq!(
            Error::InvalidMutation("bad revision".into()).recovery_action(),
            Action::Reject
        );
    }

    #[test]
    fn execution_transition_decision_table_fails_closed() {
        // Test design — Causes: every execution-state pair and the Idle/Running
        // activity-admission partition are evaluated. Effects: permitted pairs
        // mutate atomically and terminal entry clears activities; forbidden pairs
        // preserve the aggregate. Constraints/invariants: Terminated is absorbing,
        // activity begins only from Idle/Running, and no rejected edge mutates.
        // Decision rule X1: the explicit closed transition relation below is true
        // iff `can_transition_to` and `transition_execution` admit the same edge.
        use SessionExecutionState as State;

        let states = [
            State::Preparing,
            State::Activating,
            State::ActivationFailed,
            State::Running,
            State::Rescheduling,
            State::Idle,
            State::Terminated,
        ];
        for from in states {
            // Activity-admission decision table: Idle opens the first interval;
            // Running joins/replaces a live or crash-orphaned epoch; every
            // realization/reschedule/terminal state rejects before Runtime.
            // FMECA: classifying only Idle as ready strands a crash-orphaned
            // Running activity, while admitting any other state bypasses
            // realization or terminal fencing.
            assert_eq!(
                from.admits_activity(),
                matches!(from, State::Idle | State::Running),
                "activity admission from {from}"
            );
            for to in states {
                let expected = from == to
                    || (!from.is_terminal()
                        && match to {
                            State::Terminated => true,
                            State::ActivationFailed => from != State::Idle,
                            State::Activating => matches!(
                                from,
                                State::Preparing | State::Running | State::Rescheduling
                            ),
                            State::Idle => matches!(
                                from,
                                State::Preparing
                                    | State::Activating
                                    | State::Running
                                    | State::Rescheduling
                            ),
                            State::Running => from == State::Idle,
                            State::Rescheduling => matches!(from, State::Idle | State::Running),
                            State::Preparing => false,
                        });
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
            }
        }

        let mut value = session("session-1", SessionRevision(1));
        assert_eq!(value.transition_execution(State::Idle), Ok(false));
        assert_eq!(value.transition_execution(State::Running), Ok(true));
        assert_eq!(value.execution, State::Running);
        assert_eq!(value.begin_activity_epoch(), Some(1));
        assert_eq!(value.active_activity_epochs, BTreeSet::from([1]));
        assert_eq!(value.transition_execution(State::Terminated), Ok(true));
        assert!(
            value.active_activity_epochs.is_empty(),
            "terminal transition clears every active activity"
        );
        let terminal = value.clone();
        assert_eq!(
            value.transition_execution(State::Idle),
            Err(SessionExecutionTransitionError {
                from: State::Terminated,
                to: State::Idle,
            })
        );
        assert_eq!(value, terminal, "rejected transition must be atomic");
    }

    #[test]
    fn disposition_is_orthogonal_to_execution_and_delete_is_idempotent() {
        let mut archived = session("archived", SessionRevision(1));
        assert_eq!(archived.archive("2026-08-08T00:00:00Z"), Ok(true));
        assert_eq!(archived.execution, SessionExecutionState::Terminated);
        assert_eq!(archived.archived_at(), Some("2026-08-08T00:00:00Z"));
        assert!(archived.request_delete());
        assert!(archived.is_hidden());
        assert!(!archived.request_delete(), "delete replay is idempotent");

        let mut failed = session("failed", SessionRevision(1));
        failed.execution = SessionExecutionState::ActivationFailed;
        assert!(failed.is_terminal());
        assert!(
            failed.is_publicly_readable(),
            "failed async creation remains exactly queryable"
        );
        assert!(failed.request_delete(), "failed Sessions remain deletable");
        assert_eq!(failed.execution, SessionExecutionState::ActivationFailed);
        assert!(matches!(failed.disposition, SessionDisposition::Deleting));
    }

    /// Cause graph: lifecycle fact -> terminal classification -> realization and
    /// Resource-cleanup eligibility. `pending` means the Resource aggregate owns
    /// unfinished work; `active` means it has a resident manifest.
    ///
    /// | Rule | status | pending | active | Terminal | Resource reconcile |
    /// |---|---|---|---|---|---|
    /// | L1 | preparing | false | true | false | false |
    /// | L2 | running | false | true | false | false |
    /// | L3 | rescheduling | false | true | false | false |
    /// | L4 | idle | false | true | false | false |
    /// | L5 | idle | true | any | false | true |
    /// | L6 | terminated | false | true | true | true |
    /// | L7 | terminated | false | false | true | false |
    /// | L8 | deleted | false | false | true | true |
    /// | L9 | activation_failed | false | true | true | true |
    #[test]
    fn terminal_state_classification_follows_the_decision_table() {
        for (rule, status, disposition, pending, active, terminal, resource_reconcile) in [
            (
                "L1",
                "preparing",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L2",
                "running",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L3",
                "rescheduling",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L4",
                "idle",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L5",
                "idle",
                SessionDisposition::Active,
                true,
                false,
                false,
                true,
            ),
            (
                "L6",
                "terminated",
                SessionDisposition::Archived {
                    archived_at: "at".into(),
                },
                false,
                true,
                true,
                true,
            ),
            (
                "L7",
                "terminated",
                SessionDisposition::Archived {
                    archived_at: "at".into(),
                },
                false,
                false,
                true,
                false,
            ),
            (
                "L8",
                "terminated",
                SessionDisposition::Deleting,
                false,
                false,
                true,
                true,
            ),
            (
                "L9",
                "activation_failed",
                SessionDisposition::Active,
                false,
                true,
                true,
                true,
            ),
        ] {
            let mut value = session("session-1", SessionRevision(1));
            value.execution = status.parse().expect("fixture execution state");
            value.disposition = disposition;
            let desired = crate::ResolvedSessionResources::try_new(
                vec![crate::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::from("input-1"),
                    mount_path: "input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    source: crate::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-1"),
                    },
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap();
            if active {
                value
                    .resources
                    .prepare("session-1", desired.clone())
                    .expect("active generation prepare");
                value.resources.start_attempt().expect("active attempt");
                value.resources.commit().expect("active commit");
            }
            if pending {
                value
                    .resources
                    .prepare("session-1", desired)
                    .expect("L5 pending generation");
            }
            assert_eq!(value.is_terminal(), terminal, "{rule}");
            assert_eq!(
                value.needs_resource_reconciliation(),
                resource_reconcile,
                "{rule}"
            );
        }
    }

    /// Cause-effect graph:
    ///
    /// C1 key present -> C2 hash present -> C3 Session id present
    /// -> C4 next revision exists -> C5 payload revision is exact
    /// -> C6 every lifecycle fact targets the same Session -> E1 next revision.
    /// Every failed cause yields its stable E2 validation error and no write.
    ///
    /// Decision table (`-` means evaluation already terminated):
    ///
    /// | Rule | Kind | C1 | C2 | C3 | C4 | C5 | C6 | Result |
    /// |---|---|---|---|---|---|---|---|---|
    /// | R1 | replace | T | T | T | T | T | T | revision 8 |
    /// | R2 | delete | T | T | T | T | T | T | revision 8 |
    /// | R3 | either | F | - | - | - | - | - | empty key |
    /// | R4 | either | T | F | - | - | - | - | empty hash |
    /// | R5 | either | T | T | F | - | - | - | empty id |
    /// | R6 | either | T | T | T | F | - | - | exhausted |
    /// | R7 | replace | T | T | T | T | F | - | replace mismatch |
    /// | R8 | delete | T | T | T | T | F | - | tombstone mismatch |
    /// | R9 | either | T | T | T | T | T | F | lifecycle mismatch |
    ///
    /// Constraint/invariant: validation is ordered and side-effect free; the
    /// first failed prerequisite returns its stable error and never advances a
    /// Session revision or partially accepts lifecycle facts.
    #[test]
    fn mutation_validation_tests_are_generated_from_the_decision_table() {
        let rules = [
            Rule {
                id: "R1",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R2",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R3",
                payload: PayloadKind::Replace,
                key_nonempty: false,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyIdempotencyKey),
            },
            Rule {
                id: "R4",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: false,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyPayloadHash),
            },
            Rule {
                id: "R5",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: false,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptySessionId),
            },
            Rule {
                id: "R6",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: false,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::RevisionExhausted),
            },
            Rule {
                id: "R7",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::ReplacementRevisionMismatch),
            },
            Rule {
                id: "R8",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::TombstoneRevisionMismatch),
            },
            Rule {
                id: "R9",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: false,
                expected: Err(SessionMutationValidationError::LifecycleSessionMismatch),
            },
        ];

        for rule in rules {
            let expected_revision = if rule.revision_available {
                SessionRevision(7)
            } else {
                SessionRevision(u64::MAX)
            };
            let session_id = if rule.session_id_nonempty {
                "session-1"
            } else {
                ""
            };
            let next = expected_revision.0.checked_add(1).unwrap_or_default();
            let payload = match rule.payload {
                PayloadKind::Replace => SessionMutationPayload::Replace(session(
                    session_id,
                    if rule.payload_revision_exact {
                        expected_revision
                    } else {
                        SessionRevision(expected_revision.0.saturating_sub(1))
                    },
                )),
                PayloadKind::Delete => SessionMutationPayload::Delete(SessionTombstone {
                    session_id: session_id.into(),
                    deleted_revision: if rule.payload_revision_exact {
                        SessionRevision(next)
                    } else {
                        expected_revision
                    },
                    deleted_at: "2026-07-25T00:00:00Z".into(),
                }),
            };
            let mutation = SessionMutation {
                expected_revision,
                idempotency: IdempotencyRecord {
                    key: if rule.key_nonempty { "request-1" } else { "" }.into(),
                    payload_hash: if rule.hash_nonempty {
                        "sha256:payload"
                    } else {
                        ""
                    }
                    .into(),
                },
                payload,
                lifecycle_facts: vec![ManagedLifecycleFact {
                    id: "fact-1".into(),
                    object_id: if rule.lifecycle_session_exact {
                        session_id
                    } else {
                        "another-session"
                    }
                    .into(),
                    workspace_id: Some("workspace".into()),
                    event_type: "session.updated".into(),
                    timestamp: 1,
                    runtime_interval: None,
                }],
            };
            assert_eq!(
                mutation.validate(),
                rule.expected,
                "decision rule {}",
                rule.id
            );
        }
    }

    #[test]
    fn runtime_interval_mutation_decision_table_is_fail_closed() {
        // Cause/effect graph: C1=open interval with Running/non-Running state;
        // C2=closed payload absent/present; C3=event kind and stable id exact;
        // C4=end precedes start. Effects: E1 admits the exact aggregate/fact;
        // E2 rejects malformed or contradictory durable truth. Decision rules:
        // R1 Running+C1 => E1; R2 non-Running+C1 => E2; R3 exact C2+C3+!C4
        // => E1; R4-R6 missing/wrong-id/reversed payload => E2; C5 active
        // epochs are positive, no newer than the monotonic fence, and absent
        // from Idle or terminal state. R7 valid C5 => E1; R8/R9 future/Idle
        // active epochs => E2. This is the root-store boundary, so no adapter
        // can persist a billable parallel truth.
        // Constraints/invariants: open-interval state, active epochs, and the
        // matching closure fact form one atomic aggregate and one billing truth.
        let expected_revision = SessionRevision(7);
        let valid_interval = crate::SessionRuntimeInterval {
            interval_id: "interval-1".into(),
            activity_epoch: 3,
            started_at_unix_ms: 100,
            ended_at_unix_ms: 200,
            opened_revision: SessionRevision(7),
            closed_revision: SessionRevision(8),
            observations: Vec::new(),
            usage: Default::default(),
            max_list_cost_minor: None,
        };
        let mutation = |session: PersistedSession, fact: ManagedLifecycleFact| SessionMutation {
            expected_revision,
            idempotency: IdempotencyRecord {
                key: format!("interval:{}", fact.id),
                payload_hash: "payload".into(),
            },
            payload: SessionMutationPayload::Replace(session),
            lifecycle_facts: vec![fact],
        };
        let fact = |interval: Option<crate::SessionRuntimeInterval>| ManagedLifecycleFact {
            id: "interval-1".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.runtime_interval_closed".into(),
            timestamp: 1,
            runtime_interval: interval,
        };

        let mut running = session("session-1", expected_revision);
        running.execution = SessionExecutionState::Running;
        running.activity_epoch = 3;
        running.active_activity_epochs.insert(3);
        assert!(running.begin_runtime_interval(100), "R1 setup");
        let ordinary_fact = ManagedLifecycleFact {
            id: "ordinary".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.updated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        assert_eq!(
            mutation(running.clone(), ordinary_fact.clone()).validate(),
            Ok(SessionRevision(8)),
            "R1"
        );

        let mut idle_with_interval = running.clone();
        idle_with_interval.execution = SessionExecutionState::Idle;
        assert_eq!(
            mutation(idle_with_interval, fact(Some(valid_interval.clone()))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalStateMismatch),
            "R2"
        );
        let mut closed = running;
        closed.running_interval = None;
        closed.execution = SessionExecutionState::Idle;
        closed.active_activity_epochs.clear();
        assert_eq!(
            mutation(closed.clone(), fact(Some(valid_interval.clone()))).validate(),
            Ok(SessionRevision(8)),
            "R3"
        );
        let mut future_active = closed.clone();
        future_active.execution = SessionExecutionState::Running;
        future_active.active_activity_epochs.insert(4);
        assert_eq!(
            mutation(future_active, ordinary_fact.clone()).validate(),
            Err(SessionMutationValidationError::ActiveActivityStateMismatch),
            "R8"
        );
        let mut idle_active = closed.clone();
        idle_active.active_activity_epochs.insert(3);
        assert_eq!(
            mutation(idle_active, ordinary_fact).validate(),
            Err(SessionMutationValidationError::ActiveActivityStateMismatch),
            "R9"
        );
        assert_eq!(
            mutation(closed.clone(), fact(None)).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R4"
        );
        let mut wrong_id = valid_interval.clone();
        wrong_id.interval_id = "other".into();
        assert_eq!(
            mutation(closed.clone(), fact(Some(wrong_id))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R5"
        );
        let mut reversed = valid_interval;
        reversed.ended_at_unix_ms = 99;
        assert_eq!(
            mutation(closed, fact(Some(reversed))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R6"
        );
    }

    #[test]
    fn effective_runtime_usage_counts_open_interval_once() {
        // Active-time cause/effect table. C1 interval is closed/open; C2 now is
        // before/equal/after start; C3 the open interval is subsequently closed
        // at the same instant. E1 closed total only; E2 clamp clock rollback;
        // E3 add open elapsed once; E4 closing preserves the same effective
        // total (no double count). Rules: T1 closed=>E1; T2 open+before=>E2;
        // T3 open+after=>E3; T4 T3 then close=>E4. Repeating a rule is exact and
        // budget reconciliation separately keeps its cumulative cursor monotonic.
        // Constraints/invariants: elapsed time is nonnegative, monotonic, and an
        // open interval is included at most once before or after closure.
        let mut value = session("active-time", SessionRevision(1));
        value.runtime_active_millis = 2_000;
        assert_eq!(
            value.effective_runtime_active_millis(50_000),
            2_000,
            "T1/E1"
        );
        value.execution = SessionExecutionState::Running;
        value.activity_epoch = 1;
        value.active_activity_epochs.insert(1);
        assert!(value.begin_runtime_interval(10_000), "T2 setup");
        assert_eq!(value.effective_runtime_active_millis(9_000), 2_000, "T2/E2");
        assert_eq!(
            value.effective_runtime_active_millis(13_500),
            5_500,
            "T3/E3"
        );
        value.close_runtime_interval(13_500).expect("T4 close");
        assert_eq!(
            value.effective_runtime_active_millis(99_000),
            5_500,
            "T4/E4"
        );
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        SessionDeleteDispositionClass, SessionExecutionState, session_delete_request_plan,
        session_tombstone_is_admitted,
    };

    #[kani::proof]
    fn terminal_execution_never_reopens() {
        let terminal = if kani::any::<bool>() {
            SessionExecutionState::ActivationFailed
        } else {
            SessionExecutionState::Terminated
        };
        let next = match kani::any::<u8>() % 7 {
            0 => SessionExecutionState::Preparing,
            1 => SessionExecutionState::Activating,
            2 => SessionExecutionState::ActivationFailed,
            3 => SessionExecutionState::Running,
            4 => SessionExecutionState::Rescheduling,
            5 => SessionExecutionState::Idle,
            _ => SessionExecutionState::Terminated,
        };
        if terminal.can_transition_to(next) {
            assert_eq!(terminal, next);
        }
    }

    #[kani::proof]
    fn session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup() {
        let disposition_hidden = kani::any::<bool>();
        let execution_terminal = kani::any::<bool>();
        let cleanup_completed = kani::any::<bool>();
        let event_batches_complete = kani::any::<bool>();
        let session_identity_exact = kani::any::<bool>();
        let next_revision_exact = kani::any::<bool>();
        assert_eq!(
            session_tombstone_is_admitted(
                disposition_hidden,
                execution_terminal,
                cleanup_completed,
                event_batches_complete,
                session_identity_exact,
                next_revision_exact,
            ),
            disposition_hidden
                && execution_terminal
                && cleanup_completed
                && event_batches_complete
                && session_identity_exact
                && next_revision_exact
        );
    }

    #[kani::proof]
    fn session_delete_request_plan_is_exact_hidden_terminal_and_idempotent() {
        let disposition_code = kani::any::<u8>();
        kani::assume(disposition_code < 4);
        let disposition = match disposition_code {
            0 => SessionDeleteDispositionClass::Active,
            1 => SessionDeleteDispositionClass::Archived,
            2 => SessionDeleteDispositionClass::Deleting,
            3 => SessionDeleteDispositionClass::Deleted,
            _ => unreachable!(),
        };
        let execution_terminal = kani::any::<bool>();
        let plan = session_delete_request_plan(disposition, execution_terminal);
        let first_request = disposition_code < 2;
        assert_eq!(plan.transition_to_deleting, first_request);
        assert_eq!(plan.request_cleanup, first_request);
        assert_eq!(
            plan.terminalize_execution,
            first_request && !execution_terminal
        );
        if disposition_code >= 2 {
            assert!(!plan.transition_to_deleting);
            assert!(!plan.request_cleanup);
            assert!(!plan.terminalize_execution);
        }
    }
}
