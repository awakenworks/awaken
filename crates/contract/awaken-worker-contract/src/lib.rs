// @formal entrypoint worker.accepted_protocol_version_is_inside_advertised_range
// @formal entrypoint worker.never_replace_rejects_every_replacement
// @formal entrypoint worker.non_ready_state_never_accepts_work
// @formal entrypoint worker.readiness_is_probe_independent_but_dynamic_evidence_gates_claim
// @formal entrypoint worker.same_incarnation_does_not_spend_replacement_authority
// @formal entrypoint worker.sandbox_continuity_requires_a_binding
// @formal entrypoint worker.sandbox_tool_recovery_is_a_hard_claim_axis
// @formal entrypoint worker_credential.dynamic_observation_requires_exact_fact_and_half_open_lease
// @formal evidence observation_reconcile.evidence_aba_has_fresh_fence
// @formal evidence worker.placement_extension_cannot_widen_eligibility
//! Neutral worker identity, compatibility, and placement-policy contract.
//!
//! This crate deliberately separates the non-replaceable eligibility kernel from
//! replaceable ranking policy. A policy may order workers that already satisfy the
//! durable requirements, but it cannot widen authority; mutable adapters live elsewhere.

mod capability_agreement;
mod fleet_readiness;
mod manifest;
mod observation;
mod placement;
mod registry;
mod relay_route;
mod requirements;

pub use awaken_credential_contract::{
    CredentialObservationState as WorkerCredentialState, CredentialRef as WorkerCredentialRevision,
};
pub use fleet_readiness::{FleetReadiness, fleet_readiness};
pub use manifest::{
    CURRENT_CONTRACT_VERSION, CURRENT_RUNTIME_PROTOCOL_VERSION, FingerprintError,
    HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY,
    REPOSITORY_CREDENTIALS_CAPABILITY, RegisteredCapacity, SESSION_RESOURCES_CAPABILITY,
    TRUSTED_HOST_SANDBOX_BACKEND, VersionRange, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerCapacity, WorkerManifest, ZeroRegisteredCapacity,
};
#[cfg(any(test, kani))]
pub(crate) use observation::dynamic_observation_admitted;
pub use observation::{
    VerifiedAcpCapabilityEvidence, VerifiedAcpCapabilitySelection, WorkerAcpCapabilityObservation,
    WorkerAcpCapabilityRequirement, WorkerCredentialObservation, dynamic_evidence_admits,
    select_verified_acp_capability,
};
pub use placement::{
    LeastLoadedPolicy, PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE, PlacementContext, PlacementError,
    PlacementPolicy, RankedWorker, place, place_assignment,
};
pub use registry::{
    AssignmentRejection, DynamicEvidenceProbeState, RegisteredWorker, RegistryError,
    RegistryMutation, WorkerAssignment, WorkerDirectory, WorkerHeartbeat, WorkerIdentity,
    WorkerIncarnationGuard, WorkerObservationPublicationAdmission,
    WorkerObservationPublicationCompletion, WorkerObservationPublicationFence,
    WorkerObservationSource, WorkerProcessExit, WorkerRegistration,
    WorkerRegistrationHistoryAdmission, WorkerSnapshot, WorkerState, assignment_recovery_rejection,
    can_assign, process_ready_after_startup, worker_expiry_admission, worker_heartbeat_admission,
    worker_observation_sequence_after_heartbeat, worker_process_exit_admission,
    worker_registration_history_admission, worker_slot_is_replaceable,
};
pub use relay_route::{WORKER_RELAY_DESTINATION_PATH, WorkerRelayDestination};
#[cfg(any(test, kani))]
pub(crate) use requirements::required_capability_is_admitted;
pub use requirements::{
    ExecutionLocation, Incompatibility, PlacementRequirements, WorkerRecoveryMode, can_claim,
    can_claim_locally, manifest_recovery_matches_installed, sandbox_tool_recovery_is_compatible,
};

#[cfg(test)]
use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationState};
#[cfg(test)]
use awaken_provisioning_contract::{
    IsolationClass, ResourceLimits, ResourceRequests, SandboxCapabilities, SandboxRequirements,
};
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_recovery_capability() -> awaken_tool_contract::tool::ToolRecoveryCapability {
        use awaken_tool_contract::tool::ToolRecoveryCapability;
        match kani::any::<u8>() % 4 {
            0 => ToolRecoveryCapability::NonRecoverable,
            1 => ToolRecoveryCapability::ReplaySafe,
            2 => ToolRecoveryCapability::Idempotent,
            _ => ToolRecoveryCapability::DurableRequest,
        }
    }

    fn symbolic_recovery_mode() -> awaken_tool_contract::tool::ToolRecoveryMode {
        use awaken_tool_contract::tool::ToolRecoveryMode;
        match kani::any::<u8>() % 4 {
            0 => ToolRecoveryMode::NeverReplay,
            1 => ToolRecoveryMode::ReplaySafe,
            2 => ToolRecoveryMode::Idempotent,
            _ => ToolRecoveryMode::DurableRequest,
        }
    }

    fn symbolic_probe_state() -> DynamicEvidenceProbeState {
        match kani::any::<u8>() % 3 {
            0 => DynamicEvidenceProbeState::Pending,
            1 => DynamicEvidenceProbeState::Succeeded,
            _ => DynamicEvidenceProbeState::Failed,
        }
    }

    /// @obligation worker.accepted_protocol_version_is_inside_advertised_range
    #[kani::proof]
    fn accepted_version_is_inside_worker_range() {
        let min = kani::any::<u32>();
        let max = kani::any::<u32>();
        let version = kani::any::<u32>();
        let range = VersionRange { min, max };
        if range.contains(version) {
            assert!(min <= max);
            assert!(version >= min);
            assert!(version <= max);
        }
        // All u32 endpoints: valid explicit declaration && contains current
        // iff current execution is supported. Zero/default cannot grant it.
        let worker = WorkerManifest {
            runtime_protocol: range,
            ..Default::default()
        };
        assert_eq!(
            worker.supports_current_runtime_protocol(),
            min > 0
                && min <= CURRENT_RUNTIME_PROTOCOL_VERSION
                && CURRENT_RUNTIME_PROTOCOL_VERSION <= max
        );
        // New requirements use the execution protocol authority, independently
        // of the envelope version. Either location admits the exact same floor.
        let request = if kani::any::<bool>() {
            PlacementRequirements::remote_required()
        } else {
            PlacementRequirements::remote_preferred()
        };
        assert_eq!(
            request.runtime_protocol_version,
            CURRENT_RUNTIME_PROTOCOL_VERSION
        );
        assert_eq!(request.dispatch_contract_version, CURRENT_CONTRACT_VERSION);
        assert_eq!(
            worker.supports_current_runtime_protocol(),
            min > 0 && range.contains(request.runtime_protocol_version)
        );
        // Upgrade rule U1: every pre-whole-claim peer (maximum <= v3)
        // is denied even when old queued work asks for a version it supports.
        // Pin the semantic boundary, not just the mutable current constant.
        if max <= 3 {
            assert!(!worker.supports_current_runtime_protocol());
        }
    }

    /// @obligation worker.non_ready_state_never_accepts_work
    #[kani::proof]
    fn non_ready_worker_never_accepts_work() {
        let state = match kani::any::<u8>() % 4 {
            0 => WorkerState::Starting,
            1 => WorkerState::Draining,
            2 => WorkerState::Quiesced,
            _ => WorkerState::Dead,
        };
        assert!(!state.accepts_work());
    }

    /// @obligation worker.sandbox_tool_recovery_is_a_hard_claim_axis
    #[kani::proof]
    fn sandbox_tool_recovery_claim_axis_is_exact_and_non_widening() {
        use awaken_tool_contract::tool::{ToolRecoveryCapability, ToolRecoveryMode};

        let required = symbolic_recovery_mode();
        let installed = symbolic_recovery_capability();
        let expected = matches!(
            (required, installed),
            (ToolRecoveryMode::NeverReplay, _)
                | (
                    ToolRecoveryMode::ReplaySafe,
                    ToolRecoveryCapability::ReplaySafe
                )
                | (
                    ToolRecoveryMode::Idempotent,
                    ToolRecoveryCapability::Idempotent
                )
                | (
                    ToolRecoveryMode::DurableRequest,
                    ToolRecoveryCapability::DurableRequest
                )
        );
        assert_eq!(
            sandbox_tool_recovery_is_compatible(required, installed),
            expected
        );
    }

    /// @obligation worker.sandbox_tool_recovery_is_a_hard_claim_axis
    #[kani::proof]
    fn manifest_recovery_mapping_accepts_only_the_exact_installed_capability() {
        let advertised = symbolic_recovery_capability();
        let installed = symbolic_recovery_capability();
        assert_eq!(
            manifest_recovery_matches_installed(advertised, installed),
            advertised == installed
        );
    }

    /// @obligation worker.readiness_is_probe_independent_but_dynamic_evidence_gates_claim
    #[kani::proof]
    fn dynamic_evidence_can_only_restrict_ready_worker_admission() {
        let process_and_static_eligible = kani::any::<bool>();
        let credential_evidence_satisfied = kani::any::<bool>();
        let acp_evidence_satisfied = kani::any::<bool>();
        let admitted = dynamic_evidence_admits(
            process_and_static_eligible,
            credential_evidence_satisfied,
            acp_evidence_satisfied,
        );

        assert_eq!(
            admitted,
            process_and_static_eligible && credential_evidence_satisfied && acp_evidence_satisfied
        );
        if admitted {
            assert!(process_and_static_eligible);
        }
        assert!(!dynamic_evidence_admits(
            process_and_static_eligible,
            false,
            acp_evidence_satisfied,
        ));
        assert!(!dynamic_evidence_admits(
            process_and_static_eligible,
            credential_evidence_satisfied,
            false,
        ));
    }

    /// @obligation worker.readiness_is_probe_independent_but_dynamic_evidence_gates_claim
    /// @obligation worker_credential.probe_failure_does_not_drain_worker
    #[kani::proof]
    fn process_readiness_after_startup_is_probe_independent() {
        assert!(process_ready_after_startup(symbolic_probe_state()));
    }

    /// @obligation worker_credential.dynamic_observation_requires_exact_fact_and_half_open_lease
    /// @obligation worker_credential.expired_observation_never_executes
    #[kani::proof]
    fn worker_dynamic_observation_requires_exact_fact_and_half_open_lease() {
        let exact_verified_fact = kani::any::<bool>();
        let observed_at_ms = kani::any::<u64>();
        let now_ms = kani::any::<u64>();
        let valid_until_ms = kani::any::<u64>();
        let admitted = dynamic_observation_admitted(
            exact_verified_fact,
            observed_at_ms,
            now_ms,
            valid_until_ms,
        );
        assert_eq!(
            admitted,
            exact_verified_fact && observed_at_ms <= now_ms && now_ms < valid_until_ms
        );
        if admitted {
            assert!(exact_verified_fact);
            assert!(observed_at_ms <= now_ms);
            assert!(now_ms < valid_until_ms);
        }
    }

    /// @obligation worker.never_replace_rejects_every_replacement
    #[kani::proof]
    fn never_replace_rejects_every_replacement() {
        let sandbox_bound = kani::any::<bool>();
        assert!(matches!(
            assignment_recovery_rejection(true, WorkerRecoveryMode::NeverReplace, sandbox_bound,),
            Some(AssignmentRejection::ReplacementForbidden)
        ));
    }

    /// @obligation worker.sandbox_continuity_requires_a_binding
    #[kani::proof]
    fn sandbox_continuity_authorizes_replacement_exactly_when_bound() {
        let sandbox_bound = kani::any::<bool>();
        assert_eq!(
            assignment_recovery_rejection(
                true,
                WorkerRecoveryMode::RequireSandboxContinuity,
                sandbox_bound,
            )
            .is_none(),
            sandbox_bound
        );
    }

    /// @obligation worker.same_incarnation_does_not_spend_replacement_authority
    #[kani::proof]
    fn same_incarnation_never_spends_replacement_authority() {
        let recovery = match kani::any::<u8>() % 3 {
            0 => WorkerRecoveryMode::RebuildFromCommittedTruth,
            1 => WorkerRecoveryMode::RequireSandboxContinuity,
            _ => WorkerRecoveryMode::NeverReplace,
        };
        assert!(assignment_recovery_rejection(false, recovery, kani::any()).is_none());
    }

    /// @obligation session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
    #[kani::proof]
    fn required_worker_capability_cannot_be_omitted() {
        let requirement_present: bool = kani::any();
        let capability_offered: bool = kani::any();
        let admitted = required_capability_is_admitted(requirement_present, capability_offered);
        assert_eq!(admitted, !requirement_present || capability_offered);
        if requirement_present && !capability_offered {
            assert!(!admitted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn manifest(id: &str, load: u32) -> WorkerSnapshot {
        let mut manifest = WorkerManifest {
            build_digest: id.to_string(),
            capabilities: BTreeSet::from(["mcp".to_string(), "github".to_string()]),
            zone: Some("cn-a".to_string()),
            sandbox: SandboxCapabilities {
                process_tree_supervision: false,
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                enforced_network_allowlist: true,
                secret_egress_substitution: true,
                resource_limits: true,
                custom_rootfs: true,
                package_provisioning: false,
                control_services: Default::default(),
            },
            sandbox_backends: BTreeSet::from(["kubernetes".to_string()]),
            dispatch_contract: VersionRange { min: 1, max: 2 },
            runtime_protocol: VersionRange {
                min: 1,
                max: CURRENT_RUNTIME_PROTOCOL_VERSION,
            },
            checkpoint_formats: BTreeSet::from(["stream-v1".to_string()]),
            capacity: WorkerCapacity {
                max_concurrent: 4,
                ..WorkerCapacity::default()
            },
            ..WorkerManifest::default()
        };
        manifest.architecture = "x86_64".to_string();
        let capability_fingerprint = manifest.fingerprint().unwrap();
        WorkerSnapshot {
            identity: WorkerIdentity::new(id, format!("boot-{id}"), 1),
            state: WorkerState::Ready,
            manifest,
            capability_fingerprint,
            in_flight: load,
            warm_environment_shapes: BTreeSet::new(),
            credential_observations: BTreeSet::new(),
            acp_capability_observations: Vec::new(),
            expires_at_ms: 1_000,
        }
    }

    fn requirements() -> PlacementRequirements {
        PlacementRequirements {
            required_capabilities: BTreeSet::from(["github".to_string()]),
            required_zone: Some("cn-a".to_string()),
            architecture: Some("x86_64".to_string()),
            sandbox: SandboxRequirements {
                process_tree_supervision: false,
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                enforced_network_allowlist: true,
                resource_limits: true,
                custom_rootfs: true,
                package_provisioning: false,
                control_services: Default::default(),
            },
            sandbox_backend: Some("kubernetes".to_string()),
            dispatch_contract_version: 1,
            runtime_protocol_version: 2,
            checkpoint_format: Some("stream-v1".to_string()),
            ..PlacementRequirements::remote_required()
        }
    }

    fn context(requirements: PlacementRequirements) -> PlacementContext {
        PlacementContext {
            run_id: "run-1".to_string(),
            workspace_id: "ws-1".to_string(),
            requirements,
            recovered: false,
            previous_worker: None,
            attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn worker_credential_evidence_reuses_the_canonical_credential_identity_and_state() {
        // Cause/effect decision table: R1 a canonical CredentialRef is accepted by
        // Worker placement without translation; R2 the canonical observation state
        // is stored unchanged. This compile-time assignment prevents a second Worker
        // identity/state representation from returning.
        let canonical = awaken_credential_contract::CredentialRef {
            id: "cred:canonical".into(),
            revision: 11,
        };
        let worker_key: WorkerCredentialRevision = canonical.clone();
        let canonical_again: awaken_credential_contract::CredentialRef = worker_key.clone();
        assert_eq!(canonical_again, canonical);

        let state = awaken_credential_contract::CredentialObservationState::Available;
        let worker_state: WorkerCredentialState = state;
        assert_eq!(worker_state, state);
        assert!(
            WorkerCredentialObservation::available(worker_key, 5, 10)
                .is_selectable_at(&canonical, 5)
        );
    }

    #[test]
    fn full_manifest_satisfies_full_requirements() {
        // Decision-table success row: every protocol, capability, topology and
        // Sandbox cause is present, therefore claim admission has no rejection
        // effect. Negative rows are partitioned by the two tests below.
        assert!(can_claim(&manifest("a", 0).manifest, &requirements()).is_ok());
    }

    #[test]
    fn required_capability_decision_table_is_total_and_exact() {
        // Cause/effect graph: C1 the placement requires session-resources/v1;
        // C2 the immutable Worker manifest offers it. E1 admission succeeds
        // when !C1 or C2; E2 C1+!C2 returns the exact MissingCapability error.
        // Constraint: this hard axis runs inside can_claim before ranking and is
        // rechecked by the atomic claim path. Pin all other admission causes
        // to the existing compatible fixtures, including explicit current
        // runtime support; omitted protocol metadata must not grant authority.
        // The four rules exhaust C1/C2 without weakening that independent gate.
        for requirement_present in [false, true] {
            for capability_offered in [false, true] {
                let mut worker = manifest("capability-table", 0).manifest;
                let mut required = requirements();
                if requirement_present {
                    required
                        .required_capabilities
                        .insert(SESSION_RESOURCES_CAPABILITY.to_string());
                }
                if capability_offered {
                    worker
                        .capabilities
                        .insert(SESSION_RESOURCES_CAPABILITY.to_string());
                }

                let result = can_claim(&worker, &required);
                assert_eq!(result.is_ok(), !requirement_present || capability_offered);
                assert_eq!(
                    required_capability_is_admitted(requirement_present, capability_offered),
                    result.is_ok()
                );
                if requirement_present && !capability_offered {
                    assert_eq!(
                        result,
                        Err(Incompatibility::MissingCapability(
                            SESSION_RESOURCES_CAPABILITY.to_string()
                        ))
                    );
                }
            }
        }
    }

    #[test]
    fn current_runtime_requires_explicit_support_even_for_old_queued_work() {
        // Causes: C1 valid explicit range containing current protocol;
        // C2 the durable queued requirement lies inside that same range.
        // E1 claim iff C1 && C2. Otherwise E2 reject, without rewriting the
        // requirement. ANY/omitted and zero-based ranges never prove C1.
        // Rows cover old, current, future, inverted and broad declarations;
        // each crosses legacy/current/future queued versions 0..=current+1.
        // Regression R3: v3 peers ACK individual scheduled stages; whole-claim
        // retirement makes that ACK final. Both v3-pinned and current queued
        // work must reject those peers. The existing matrix owns these rows
        // rather than a second stage-protocol compatibility test/policy.
        for (range, current) in [
            (VersionRange::ANY, false),
            (VersionRange { min: 0, max: 3 }, false),
            (VersionRange { min: 3, max: 2 }, false),
            (VersionRange::exact(1), false),
            (VersionRange { min: 1, max: 2 }, false),
            (VersionRange { min: 1, max: 3 }, false),
            (VersionRange::exact(3), false),
            (
                VersionRange {
                    min: 1,
                    max: CURRENT_RUNTIME_PROTOCOL_VERSION,
                },
                true,
            ),
            (VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION), true),
            (
                VersionRange {
                    min: 2,
                    max: u32::MAX,
                },
                true,
            ),
            (
                VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION + 1),
                false,
            ),
        ] {
            let mut worker = manifest("protocol", 0).manifest;
            worker.runtime_protocol = range;
            assert_eq!(worker.supports_current_runtime_protocol(), current);
            for version in 0..=CURRENT_RUNTIME_PROTOCOL_VERSION + 1 {
                let mut queued = requirements();
                queued.runtime_protocol_version = version;
                assert_eq!(
                    can_claim(&worker, &queued).is_ok(),
                    current && range.contains(version),
                    "{range:?}/{version}"
                );
                assert_eq!(queued.runtime_protocol_version, version);
            }
        }
        let mut encoded = serde_json::to_value(WorkerManifest::default()).unwrap();
        encoded.as_object_mut().unwrap().remove("runtime_protocol");
        let omitted: WorkerManifest = serde_json::from_value(encoded).unwrap();
        assert!(!omitted.supports_current_runtime_protocol());
    }

    #[test]
    fn terminal_cleanup_requirements_exclude_run_only_capabilities() {
        // Cause/effect graph: C1 cleanup retains any Session Resource; C2 one
        // retained Repository has a credential binding; C3 one durable
        // checkpoint names a format. Effects: E1 require the Session Resource
        // adapter; E2 additionally require Repository credential injection; E3
        // require the exact checkpoint format; E4 always require explicit
        // current runtime while model/ACP/tool/credential sets remain empty.
        //
        // | Rule | C1 resources | C2 credentialed repo | C3 checkpoint | Effect |
        // |---|---|---|---|---|
        // | T1 | no | no | no | E4 only |
        // | T2 | yes | no | no | E1 + E4 |
        // | T3 | yes | yes | stream-v1 | E1 + E2 + E3 + E4 |
        let bare = PlacementRequirements::terminal_cleanup(false, false, None);
        assert_eq!(
            bare.runtime_protocol_version, CURRENT_RUNTIME_PROTOCOL_VERSION,
            "T1/E4"
        );
        assert!(bare.required_capabilities.is_empty(), "T1/E4");
        assert!(bare.required_credentials.is_empty(), "T1/E4");
        assert!(bare.required_acp_capabilities.is_empty(), "T1/E4");
        assert!(bare.required_sandbox_tool_recovery.is_empty(), "T1/E4");

        let resources = PlacementRequirements::terminal_cleanup(true, false, None);
        assert_eq!(
            resources.required_capabilities,
            BTreeSet::from([SESSION_RESOURCES_CAPABILITY.to_string()]),
            "T2/E1"
        );

        let complete =
            PlacementRequirements::terminal_cleanup(true, true, Some("stream-v1".into()));
        assert!(
            complete
                .required_capabilities
                .contains(SESSION_RESOURCES_CAPABILITY),
            "T3/E1"
        );
        assert!(
            complete
                .required_capabilities
                .contains(REPOSITORY_CREDENTIALS_CAPABILITY),
            "T3/E2"
        );
        assert_eq!(
            complete.checkpoint_format.as_deref(),
            Some("stream-v1"),
            "T3/E3"
        );
    }

    #[test]
    fn sandbox_tool_recovery_is_a_hard_claim_axis() {
        use awaken_tool_contract::tool::{ToolRecoveryCapability, ToolRecoveryMode};

        // Cause/effect decision table:
        // C1=a snapshot demands no non-default Sandbox recovery; C2=it demands
        // DurableRequest; C3=the Worker truthfully advertises DurableRequest.
        // R1 !C2 => any executor remains eligible; R2 C2+C3 => eligible; R3
        // C2+!C3 => fail closed before ranking with the exact mismatch. This
        // keeps deployment drift Pending instead of entering an incompatible
        // SessionEnvironment and discovering the mismatch during tool use.
        let mut worker = manifest("recovery-worker", 0).manifest;
        let mut required = requirements();
        assert!(can_claim(&worker, &required).is_ok(), "R1");

        required
            .required_sandbox_tool_recovery
            .insert(ToolRecoveryMode::DurableRequest);
        worker.sandbox_tool_recovery = ToolRecoveryCapability::DurableRequest;
        assert!(can_claim(&worker, &required).is_ok(), "R2");

        worker.sandbox_tool_recovery = ToolRecoveryCapability::NonRecoverable;
        assert_eq!(
            can_claim(&worker, &required),
            Err(Incompatibility::SandboxToolRecovery {
                required: ToolRecoveryMode::DurableRequest,
                actual: ToolRecoveryCapability::NonRecoverable,
            }),
            "R3"
        );
    }

    #[test]
    fn registered_capacity_has_exactly_one_source_and_no_silent_zero() {
        // Cause/effect decision table for the one currency the execution pool
        // installer accepts. C1 the manifest registers a positive capacity; C2 it
        // registers zero. R1 C1 => a registration carrying exactly the published
        // number, readable once and consumable once; R2 C2 => no registration at
        // all, so the installer can never be handed a clamped placeholder for a
        // capacity the scheduler was never told about.
        //
        // This is the type-level half of the "registered but unconsumed"
        // guard: `of` is the only constructor, so a slot count that no manifest
        // published cannot reach the executable Worker constructor, and `into_slots`
        // consumes by value so a registration that is read but never installed
        // is a compiler diagnostic rather than a silent divergence.
        let mut manifest = manifest("capacity-worker", 0).manifest;
        for published in [1_u32, 4, 97] {
            manifest.capacity.max_concurrent = published;
            let registered = RegisteredCapacity::of(&manifest).expect("R1");
            assert_eq!(registered.slots().get(), published as usize, "R1 exact");
            assert_eq!(
                registered.into_slots().get(),
                published as usize,
                "R1 installs the published number"
            );
        }

        manifest.capacity.max_concurrent = 0;
        assert!(
            matches!(
                RegisteredCapacity::of(&manifest),
                Err(ZeroRegisteredCapacity)
            ),
            "R2"
        );
    }

    #[test]
    fn resource_demand_is_a_hard_worker_eligibility_axis() {
        // Cause/effect decision table: C1 Worker ceiling is entirely delegated;
        // C2 every demanded axis has an explicit ceiling; C3 every ceiling is
        // large enough. R1 C1 => backend decides and Worker remains eligible;
        // R2 !C1+C2+C3 => eligible; R3 !C1+!C2 and R4 !C1+C2+!C3 =>
        // InsufficientResources. Ranking is not invoked for rejected rows.
        let mut worker = manifest("resource-worker", 0).manifest;
        assert!(can_claim(&worker, &requirements()).is_ok(), "R1");

        let mut required = requirements();
        required.resources = ResourceRequests {
            cpu_millis: Some(1_000),
            memory_bytes: Some(1 << 30),
            disk_bytes: None,
        };
        assert!(can_claim(&worker, &required).is_ok(), "R1 delegated");

        worker.capacity.resources = ResourceLimits {
            cpu_millis: Some(1_000),
            memory_bytes: Some(1 << 30),
            pids: None,
            disk_bytes: None,
        };
        assert!(can_claim(&worker, &required).is_ok(), "R2");
        worker.capacity.resources.memory_bytes = None;
        assert_eq!(
            can_claim(&worker, &required),
            Err(Incompatibility::InsufficientResources),
            "R3"
        );
        worker.capacity.resources.memory_bytes = Some((1 << 30) - 1);
        assert_eq!(
            can_claim(&worker, &required),
            Err(Incompatibility::InsufficientResources),
            "R4"
        );
    }

    #[test]
    fn every_sandbox_capability_axis_fails_closed_through_one_predicate() {
        // Cause/effect graph: each SandboxRequirements bit is a conjunctive cause;
        // E1=all are satisfied -> claimable; E2=any one absent -> the single
        // SandboxCapabilities incompatibility. Isolation is ordered, boolean axes
        // are implication constraints, and unrelated Worker axes stay fixed.
        //
        // Decision table: R1 full vector -> accept (owned by the preceding test);
        // R2 weaker isolation -> reject; R3..R10 one missing enforcement bit ->
        // reject. Package provisioning is added to both sides for its positive row
        // before being removed from the Worker for its negative row.
        let required = requirements();
        let assert_rejected = |worker: WorkerManifest, rule: &str| {
            assert_eq!(
                can_claim(&worker, &required),
                Err(Incompatibility::SandboxCapabilities),
                "{rule}"
            );
        };

        let mut worker = manifest("a", 0).manifest;
        worker.sandbox.isolation = IsolationClass::Namespace;
        assert_rejected(worker, "R2 isolation");

        for axis in [
            "tool_transparent",
            "path_fidelity",
            "enforced_readonly",
            "network_isolation",
            "enforced_network_allowlist",
            "resource_limits",
            "custom_rootfs",
        ] {
            let mut worker = manifest("a", 0).manifest;
            match axis {
                "tool_transparent" => worker.sandbox.tool_transparent = false,
                "path_fidelity" => worker.sandbox.path_fidelity = false,
                "enforced_readonly" => worker.sandbox.enforced_readonly = false,
                "network_isolation" => worker.sandbox.network_isolation = false,
                "enforced_network_allowlist" => worker.sandbox.enforced_network_allowlist = false,
                "resource_limits" => worker.sandbox.resource_limits = false,
                "custom_rootfs" => worker.sandbox.custom_rootfs = false,
                _ => unreachable!(),
            }
            assert_rejected(worker, axis);
        }

        let mut package_required = required;
        package_required.sandbox.package_provisioning = true;
        let worker = manifest("a", 0).manifest;
        assert_eq!(
            can_claim(&worker, &package_required),
            Err(Incompatibility::SandboxCapabilities),
            "R10 package_provisioning"
        );
    }

    #[test]
    fn worker_private_credential_requires_the_exact_live_revision() {
        let required = WorkerCredentialRevision {
            id: "cred:worker".into(),
            revision: 7,
        };
        let mut requirements = requirements();
        requirements.required_credentials.insert(required.clone());

        let mut worker = manifest("a", 0);
        assert!(!worker.accepts(&requirements, 10));
        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(
                WorkerCredentialRevision {
                    id: required.id.clone(),
                    revision: 6,
                },
                9,
                100,
            ));
        assert!(!worker.accepts(&requirements, 10));
        worker
            .credential_observations
            .insert(WorkerCredentialObservation {
                credential: required.clone(),
                state: WorkerCredentialState::LoginRequired,
                observed_at_ms: 10,
                valid_until_ms: 100,
                reason_code: Some("credential_login_required".into()),
            });
        assert!(
            !worker.accepts(&requirements, 10),
            "an exact but unavailable state is not placement evidence"
        );
        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(required, 10, 100));
        assert!(worker.accepts(&requirements, 10));
    }

    #[test]
    fn worker_private_credential_observation_is_a_bounded_fact() {
        let required = WorkerCredentialRevision {
            id: "cred:worker".into(),
            revision: 7,
        };
        let mut requirements = requirements();
        requirements.required_credentials.insert(required.clone());
        let mut worker = manifest("a", 0);

        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(
                required.clone(),
                100,
                200,
            ));

        assert!(
            !worker.accepts(&requirements, 99),
            "future evidence is invalid"
        );
        assert!(
            worker.accepts(&requirements, 100),
            "lower bound is inclusive"
        );
        assert!(worker.accepts(&requirements, 199));
        assert!(
            !worker.accepts(&requirements, 200),
            "valid-until is an exclusive upper bound"
        );
    }

    #[test]
    fn dynamic_observation_admission_is_exact_and_half_open() {
        assert!(!dynamic_observation_admitted(false, 100, 100, 200));
        assert!(!dynamic_observation_admitted(true, 100, 99, 200));
        assert!(dynamic_observation_admitted(true, 100, 100, 200));
        assert!(dynamic_observation_admitted(true, 100, 199, 200));
        assert!(!dynamic_observation_admitted(true, 100, 200, 200));
        assert!(!dynamic_observation_admitted(true, 100, 100, 100));
    }

    // Cause/effect decision table for publication-pinned ACP capability:
    // A1 exact backend+fingerprint, Verified, inside TTL -> selectable.
    // A2 wrong fingerprint/backend or negative state       -> reject.
    // A3 future observation or now >= valid_until          -> reject.
    // A4 Verified state with incomplete/mixed evidence     -> reject.
    #[test]
    fn acp_capability_requires_the_exact_live_fingerprint() {
        let required = WorkerAcpCapabilityRequirement {
            backend_ref: "acp:codex".into(),
            fingerprint: "sha256:expected".into(),
        };
        let mut requirements = requirements();
        requirements
            .required_acp_capabilities
            .insert(required.clone());
        let mut worker = manifest("a", 0);
        let observation = |backend_ref: &str,
                           fingerprint: &str,
                           state: AcpCapabilityObservationState| {
            let negotiated = || awaken_acp_contract::NegotiatedAcpCapabilities {
                protocol_version: "1".into(),
                load_session: false,
                prompt_image: false,
                prompt_audio: false,
                prompt_embedded_context: false,
                mcp_http: false,
                mcp_sse: false,
                session_list: false,
                modes: Vec::new(),
                config_options: Vec::new(),
            };
            let observation = match state {
                AcpCapabilityObservationState::Verified => AcpCapabilityObservation::verified(
                    backend_ref,
                    "test",
                    100,
                    fingerprint,
                    negotiated(),
                ),
                AcpCapabilityObservationState::Unavailable => {
                    AcpCapabilityObservation::unavailable(backend_ref, "test", 100, "not_verified")
                }
                AcpCapabilityObservationState::ProbeFailed => {
                    AcpCapabilityObservation::probe_failed(backend_ref, "test", 100, "not_verified")
                }
            }
            .expect("coherent capability fixture");
            WorkerAcpCapabilityObservation {
                observation,
                valid_until_ms: 200,
            }
        };

        worker.acp_capability_observations = vec![observation(
            "acp:codex",
            "sha256:other",
            AcpCapabilityObservationState::Verified,
        )];
        assert!(!worker.accepts(&requirements, 150), "A2");
        worker.acp_capability_observations = vec![observation(
            "acp:codex",
            "sha256:expected",
            AcpCapabilityObservationState::Unavailable,
        )];
        assert!(!worker.accepts(&requirements, 150), "A2");
        worker.acp_capability_observations = vec![observation(
            "acp:codex",
            "sha256:expected",
            AcpCapabilityObservationState::Verified,
        )];
        assert!(!worker.accepts(&requirements, 99), "A3");
        assert!(worker.accepts(&requirements, 100), "A1");
        assert!(worker.accepts(&requirements, 199), "A1");
        assert!(!worker.accepts(&requirements, 200), "A3");
        let mut malformed = serde_json::to_value(&worker.acp_capability_observations[0]).unwrap();
        malformed["observation"]["negotiated"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<WorkerAcpCapabilityObservation>(malformed).is_err(),
            "A4 malformed evidence cannot enter a Worker manifest",
        );
    }

    #[test]
    fn live_acp_selection_is_worker_local_order_independent_and_fail_closed() {
        // Cause/effect graph: C1 Worker is Ready and live; C2 capability is a
        // fresh verified profile for the backend; C3 the required credential
        // is fresh on that same Worker; C4 all admitted profiles agree.
        // Effects: E1 one coherent profile is available; E2 missing/cross-Worker
        // evidence is unavailable; E3 disagreement is incompatible.
        // Decision table: R1 C1+C2+C3+C4 -> E1; R2 !C1|!C2|!C3 -> E2;
        // R3 C1+C2+C3+!C4 -> E3. Reversing input order must preserve every rule.
        let credential = WorkerCredentialRevision {
            id: "credential-1".into(),
            revision: 1,
        };
        let negotiated = || awaken_acp_contract::NegotiatedAcpCapabilities {
            protocol_version: "1".into(),
            load_session: false,
            prompt_image: false,
            prompt_audio: false,
            prompt_embedded_context: false,
            mcp_http: true,
            mcp_sse: false,
            session_list: false,
            modes: Vec::new(),
            config_options: Vec::new(),
        };
        let registered =
            |id: &str, fingerprint: &str, with_credential: bool| {
                let mut snapshot = manifest(id, 0);
                if with_credential {
                    snapshot.credential_observations.insert(
                        WorkerCredentialObservation::available(credential.clone(), 100, 200),
                    );
                }
                snapshot.acp_capability_observations = vec![WorkerAcpCapabilityObservation {
                    observation: AcpCapabilityObservation::verified(
                        "acp:claude",
                        "1",
                        100,
                        fingerprint,
                        negotiated(),
                    )
                    .unwrap(),
                    valid_until_ms: 200,
                }];
                RegisteredWorker {
                    snapshot,
                    heartbeat_sequence: 1,
                    observation_sequence: 1,
                    registered_at_ms: 1,
                    heartbeat_at_ms: 100,
                    drain_deadline_ms: None,
                    process_exit: None,
                }
            };

        let matching = registered("matching", "sha256:same", true);
        let duplicate = registered("duplicate", "sha256:same", true);
        for workers in [
            vec![matching.clone(), duplicate.clone()],
            vec![duplicate.clone(), matching.clone()],
        ] {
            assert!(
                matches!(
                    select_verified_acp_capability(&workers, "acp:claude", Some(&credential), 150),
                    VerifiedAcpCapabilitySelection::Available(_)
                ),
                "R1 order-independent duplicate evidence"
            );
        }

        let split_credential = registered("capability-only", "sha256:same", false);
        let mut credential_only = registered("credential-only", "sha256:same", true);
        credential_only.snapshot.acp_capability_observations.clear();
        assert_eq!(
            select_verified_acp_capability(
                &[split_credential, credential_only],
                "acp:claude",
                Some(&credential),
                150
            ),
            VerifiedAcpCapabilitySelection::Unavailable,
            "R2 evidence from different Workers cannot be joined"
        );

        let incompatible = registered("incompatible", "sha256:other", true);
        assert_eq!(
            select_verified_acp_capability(
                &[matching, incompatible],
                "acp:claude",
                Some(&credential),
                150
            ),
            VerifiedAcpCapabilitySelection::Incompatible,
            "R3"
        );
    }

    #[test]
    fn one_failed_credential_observation_does_not_block_an_unrelated_requirement() {
        let required = WorkerCredentialRevision {
            id: "cred:healthy".into(),
            revision: 2,
        };
        let mut requirements = requirements();
        requirements.required_credentials.insert(required.clone());
        let mut worker = manifest("a", 0);
        worker
            .credential_observations
            .insert(WorkerCredentialObservation {
                credential: WorkerCredentialRevision {
                    id: "cred:failed".into(),
                    revision: 1,
                },
                state: WorkerCredentialState::ProbeFailed,
                observed_at_ms: 100,
                valid_until_ms: 200,
                reason_code: Some("credential_probe_failed".into()),
            });
        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(required, 100, 200));

        assert!(worker.accepts(&requirements, 150));
    }

    #[test]
    fn legacy_observation_without_a_deadline_fails_closed() {
        let observation: WorkerCredentialObservation = serde_json::from_value(serde_json::json!({
            "credential": { "id": "cred:legacy", "revision": 1 },
            "state": "available",
            "observed_at_ms": 100
        }))
        .expect("legacy wire shape remains decodable");
        assert_eq!(observation.valid_until_ms, 0);
        assert!(!observation.is_selectable_at(&observation.credential, 100));
    }

    #[test]
    fn every_hard_axis_fails_closed() {
        let worker = manifest("a", 0);
        let mut cases = Vec::new();
        let mut r = requirements();
        r.required_capabilities.insert("gpu".to_string());
        cases.push(r);
        let mut r = requirements();
        r.required_zone = Some("cn-b".to_string());
        cases.push(r);
        let mut r = requirements();
        r.architecture = Some("aarch64".to_string());
        cases.push(r);
        let mut r = requirements();
        r.dispatch_contract_version = 9;
        cases.push(r);
        let mut r = requirements();
        r.runtime_protocol_version = 9;
        cases.push(r);
        let mut r = requirements();
        r.sandbox_backend = Some("firecracker".to_string());
        cases.push(r);
        let mut r = requirements();
        r.checkpoint_format = Some("unknown".to_string());
        cases.push(r);
        let mut r = requirements();
        r.location = ExecutionLocation::LocalOnly;
        cases.push(r);
        assert!(
            cases
                .iter()
                .all(|r| can_claim(&worker.manifest, r).is_err())
        );
    }

    #[test]
    fn trusted_host_backend_is_exact_not_monotonic_isolation() {
        // Cause/effect graph: C1 Worker has strong Container capabilities;
        // C2 Worker reports exact trusted-host backend. E1 C1 without C2 is
        // rejected; E2 C1+C2 is admitted. This two-rule decision table proves
        // stronger isolation cannot substitute for requested host identity.
        let worker = manifest("trusted", 0);
        let mut exact = requirements();
        exact.sandbox = SandboxRequirements::default();
        exact.sandbox_backend = Some(TRUSTED_HOST_SANDBOX_BACKEND.to_string());
        assert!(matches!(
            can_claim(&worker.manifest, &exact),
            Err(Incompatibility::SandboxBackend(backend))
                if backend == TRUSTED_HOST_SANDBOX_BACKEND
        ));

        let mut reported = worker.manifest;
        reported
            .sandbox_backends
            .insert(TRUSTED_HOST_SANDBOX_BACKEND.to_string());
        assert!(can_claim(&reported, &exact).is_ok());
    }

    #[test]
    fn supervision_matching_never_borrows_another_backends_claim() {
        // WM1 requested backend x primary guarantee x additional-host
        // guarantee/legacy omission x requirement is the complete 2x2x3x2
        // table. Effect: only the selected backend's guarantee can satisfy
        // a strict request. Legacy omission is false, never primary fallback.
        let mut manifest = manifest("supervision", 0).manifest;
        manifest
            .sandbox_backends
            .insert(TRUSTED_HOST_SANDBOX_BACKEND.into());
        for trusted in [false, true] {
            for primary in [false, true] {
                for additional in [None, Some(false), Some(true)] {
                    for required in [false, true] {
                        manifest.sandbox.process_tree_supervision = primary;
                        manifest.trusted_host_sandbox =
                            additional.map(|available| SandboxCapabilities {
                                process_tree_supervision: available,
                                ..Default::default()
                            });
                        let request = PlacementRequirements {
                            sandbox: SandboxRequirements {
                                process_tree_supervision: required,
                                ..Default::default()
                            },
                            sandbox_backend: trusted.then(|| TRUSTED_HOST_SANDBOX_BACKEND.into()),
                            ..PlacementRequirements::remote_preferred()
                        };
                        let available = if trusted {
                            additional.unwrap_or(false)
                        } else {
                            primary
                        };
                        let result = can_claim(&manifest, &request);
                        assert_eq!(
                            result.is_ok(),
                            !required || available,
                            "trusted={trusted} primary={primary} additional={additional:?} required={required}: {result:?}"
                        );
                    }
                }
            }
        }
        // WM2 a malformed host profile cannot smuggle a stronger isolation
        // identity; WM3 legacy serialization omits the new field entirely.
        manifest.trusted_host_sandbox = Some(SandboxCapabilities {
            isolation: IsolationClass::Container,
            ..Default::default()
        });
        let request = PlacementRequirements {
            sandbox_backend: Some(TRUSTED_HOST_SANDBOX_BACKEND.into()),
            ..Default::default()
        };
        assert_eq!(
            can_claim(&manifest, &request),
            Err(Incompatibility::SandboxCapabilities)
        );
        let legacy = WorkerManifest::default();
        let wire = serde_json::to_value(&legacy).unwrap();
        assert!(wire.get("trusted_host_sandbox").is_none());
        assert!(wire["sandbox"].get("process_tree_supervision").is_none());
        assert_eq!(
            serde_json::from_value::<WorkerManifest>(wire).unwrap(),
            legacy
        );
    }

    #[test]
    fn trusted_host_placement_rechecks_login_ttl_and_backend_removal() {
        // Decision graph: exact host backend AND live Worker AND available
        // exact credential => eligible. T1 all true=>claim; T2 backend removed,
        // T3 login revoked, T4 observation expired, T5 Worker expired => no
        // eligible Worker, even with a stronger Container candidate available.
        // Each negative row starts from the same successful snapshot; no row
        // can be satisfied by accidentally failing an unrelated precondition.
        let credential = WorkerCredentialRevision {
            id: "host-login".into(),
            revision: 1,
        };
        let mut required = requirements();
        required.sandbox = SandboxRequirements::default();
        required.sandbox_backend = Some(TRUSTED_HOST_SANDBOX_BACKEND.into());
        required.required_credentials.insert(credential.clone());
        let mut ready = manifest("host", 0);
        ready
            .manifest
            .sandbox_backends
            .insert(TRUSTED_HOST_SANDBOX_BACKEND.into());
        ready.capability_fingerprint = ready.manifest.fingerprint().unwrap();
        ready
            .credential_observations
            .insert(WorkerCredentialObservation::available(
                credential.clone(),
                5,
                20,
            ));
        assert!(ready.accepts(&required, 10), "T1");
        for rule in ["removed", "revoked", "credential-expired", "worker-expired"] {
            let mut worker = ready.clone();
            match rule {
                "removed" => {
                    worker
                        .manifest
                        .sandbox_backends
                        .remove(TRUSTED_HOST_SANDBOX_BACKEND);
                    worker.capability_fingerprint = worker.manifest.fingerprint().unwrap();
                }
                "revoked" => {
                    worker.credential_observations.clear();
                    worker
                        .credential_observations
                        .insert(WorkerCredentialObservation {
                            credential: credential.clone(),
                            state: WorkerCredentialState::LoginRequired,
                            observed_at_ms: 5,
                            valid_until_ms: 20,
                            reason_code: None,
                        });
                }
                "credential-expired" => {
                    worker.credential_observations.clear();
                    worker
                        .credential_observations
                        .insert(WorkerCredentialObservation::available(
                            credential.clone(),
                            5,
                            10,
                        ));
                }
                "worker-expired" => worker.expires_at_ms = 10,
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    place(
                        &LeastLoadedPolicy,
                        &context(required.clone()),
                        &[worker, manifest("cloud", 0)],
                        10
                    ),
                    Err(PlacementError::NoEligibleWorker)
                ),
                "{rule}"
            );
        }

        // T6 cause/effect exhaustive decision table: exact backend (bit 0),
        // Available login (1), exact credential revision (2), Ready state (3),
        // live Worker (4) are five independent conjuncts. For every one of 32
        // combinations, test before/at observed_at and before/at expiry. All
        // non-target requirements remain valid; a stronger cloud candidate is
        // always present so rejection cannot accidentally become fallback.
        // T7 the initial placement and later assignment revalidation consume
        // the SAME predicate at their own time; expiry between them must fail.
        for mask in 0_u8..32 {
            for now in [4, 5, 19, 20] {
                let mut worker = ready.clone();
                if mask & 1 == 0 {
                    worker
                        .manifest
                        .sandbox_backends
                        .remove(TRUSTED_HOST_SANDBOX_BACKEND);
                }
                worker.capability_fingerprint = worker.manifest.fingerprint().unwrap();
                worker.state = if mask & 8 != 0 {
                    WorkerState::Ready
                } else {
                    WorkerState::Draining
                };
                worker.expires_at_ms = if mask & 16 != 0 { 30 } else { now };
                let mut observation =
                    WorkerCredentialObservation::available(credential.clone(), 5, 20);
                if mask & 2 == 0 {
                    observation.state = WorkerCredentialState::LoginRequired;
                }
                if mask & 4 == 0 {
                    observation.credential.revision += 1;
                }
                worker.credential_observations = [observation].into();
                let admitted = mask == 31 && (5..20).contains(&now);
                let result = place(
                    &LeastLoadedPolicy,
                    &context(required.clone()),
                    &[worker.clone(), manifest("cloud", 0)],
                    now,
                );
                if admitted {
                    assert_eq!(
                        result.unwrap().identity,
                        worker.identity,
                        "T6 mask={mask} time={now}"
                    );
                } else {
                    assert!(
                        matches!(result, Err(PlacementError::NoEligibleWorker)),
                        "T6 mask={mask} time={now}: {result:?}"
                    );
                }
                assert_eq!(
                    can_assign(&worker, &required, None, false, now).is_ok(),
                    admitted,
                    "T7"
                );
            }
        }
        assert!(can_assign(&ready, &required, None, false, 19).is_ok());
        assert_eq!(
            can_assign(&ready, &required, None, false, 20),
            Err(AssignmentRejection::Ineligible),
            "T7 expiry after selection"
        );
    }

    #[test]
    fn least_loaded_is_deterministic() {
        let workers = vec![manifest("b", 1), manifest("a", 1), manifest("c", 3)];
        let selected = place(&LeastLoadedPolicy, &context(requirements()), &workers, 10).unwrap();
        assert_eq!(selected.identity.worker_id, "a");
    }

    #[test]
    fn warm_shape_is_a_soft_preference_before_load() {
        // FMECA (S=severity, O=occurrence, D=detection; 1..10):
        // F1 stale/missing receipt selects a cold Worker (S3 O4 D2, RPN24):
        // acceptable latency degradation, never an eligibility failure.
        // F2 receipt treated as hard capability (S8 O2 D3, RPN48): prevented by
        // applying it only inside ranking after the compatibility kernel.
        // F3 lower-load cold Worker defeats useful capacity (S4 O5 D2, RPN40):
        // prevented by ordering warm match before in-flight count.
        //
        // Cause-effect graph: C1=request has preferred shape; C2=worker has exact
        // receipt; C3=worker is otherwise eligible; C4=worker has lower load.
        // Effects: E1=warm worker ranks first; E2=least-loaded fallback; E3=no
        // eligible worker is excluded. Derived decision table:
        // | Rule | C1 | C2(any) | C3 | C4(cold) | Effect |
        // | P1   | 1  | 1       | 1  | 1        | E1     |
        // | P2   | 1  | 0       | 1  | 1        | E2,E3  |
        // | P3   | 0  | -       | 1  | 1        | E2,E3  |
        let mut warm = manifest("warm", 3);
        warm.warm_environment_shapes.insert("shape-a".into());
        let cold = manifest("cold", 0);
        let mut preferred = context(requirements());
        preferred.attributes.insert(
            PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE.into(),
            "shape-a".into(),
        );
        assert_eq!(
            place(&LeastLoadedPolicy, &preferred, &[cold.clone(), warm], 10)
                .unwrap()
                .identity
                .worker_id,
            "warm",
            "P1"
        );

        assert_eq!(
            place(
                &LeastLoadedPolicy,
                &preferred,
                &[cold.clone(), manifest("other", 2)],
                10
            )
            .unwrap()
            .identity
            .worker_id,
            "cold",
            "P2"
        );
        assert_eq!(
            place(
                &LeastLoadedPolicy,
                &context(requirements()),
                &[cold, manifest("other", 2)],
                10
            )
            .unwrap()
            .identity
            .worker_id,
            "cold",
            "P3"
        );
    }

    struct InjectingPolicy;
    impl PlacementPolicy for InjectingPolicy {
        fn id(&self) -> &str {
            "injecting"
        }

        fn rank(
            &self,
            _context: &PlacementContext,
            _eligible: &[WorkerSnapshot],
        ) -> Result<Vec<RankedWorker>, PlacementError> {
            Ok(vec![RankedWorker {
                identity: WorkerIdentity::new("evil", "boot", 1),
                score: i64::MAX,
                reason: "bypass".to_string(),
            }])
        }
    }

    #[test]
    fn extension_cannot_inject_an_ineligible_worker() {
        let error = place(
            &InjectingPolicy,
            &context(requirements()),
            &[manifest("a", 0)],
            10,
        )
        .unwrap_err();
        assert!(matches!(error, PlacementError::IneligibleResult(_)));
    }

    #[test]
    fn stale_draining_full_or_tampered_workers_are_filtered() {
        let mut stale = manifest("stale", 0);
        stale.expires_at_ms = 10;
        let mut draining = manifest("draining", 0);
        draining.state = WorkerState::Draining;
        let full = manifest("full", 4);
        let mut tampered = manifest("tampered", 0);
        tampered.capability_fingerprint = "sha256:bad".to_string();
        assert!(matches!(
            place(
                &LeastLoadedPolicy,
                &context(requirements()),
                &[stale, draining, full, tampered],
                10,
            ),
            Err(PlacementError::NoEligibleWorker)
        ));
    }

    #[test]
    fn legacy_defaults_and_strict_builder_are_explicit() {
        let legacy: PlacementRequirements = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.contract_version, 0);
        assert_eq!(legacy.location, ExecutionLocation::RemotePreferred);
        // C1 legacy decoder => preserve v0; C2 new required/preferred request
        // => envelope v1 and independently current runtime v3. The exact-current
        // Worker must claim both; changing an envelope must not imply protocol support.
        assert_eq!(legacy.runtime_protocol_version, 0);
        for (request, location) in [
            (
                PlacementRequirements::remote_required(),
                ExecutionLocation::RemoteRequired,
            ),
            (
                PlacementRequirements::remote_preferred(),
                ExecutionLocation::RemotePreferred,
            ),
        ] {
            assert_eq!(request.contract_version, CURRENT_CONTRACT_VERSION);
            assert_eq!(request.dispatch_contract_version, CURRENT_CONTRACT_VERSION);
            assert_eq!(
                request.runtime_protocol_version,
                CURRENT_RUNTIME_PROTOCOL_VERSION
            );
            assert_eq!(request.location, location);
            let worker = WorkerManifest {
                runtime_protocol: VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION),
                ..Default::default()
            };
            assert!(can_claim(&worker, &request).is_ok());
        }
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_manifest_changes() {
        let mut first = manifest("a", 0).manifest;
        let same = first.clone();
        assert_eq!(first.fingerprint().unwrap(), same.fingerprint().unwrap());
        let old = first.fingerprint().unwrap();
        first.capabilities.insert("gpu".to_string());
        assert_ne!(old, first.fingerprint().unwrap());
    }

    #[test]
    fn recovery_mode_controls_cross_incarnation_assignment() {
        let first = manifest("worker-a", 0);
        let replacement = manifest("worker-b", 0);
        let previous = WorkerAssignment::from(&first);

        let mut never = requirements();
        never.recovery = WorkerRecoveryMode::NeverReplace;
        assert_eq!(
            can_assign(&replacement, &never, Some(&previous), true, 10),
            Err(AssignmentRejection::ReplacementForbidden)
        );
        assert!(can_assign(&first, &never, Some(&previous), false, 10).is_ok());

        let mut continuity = requirements();
        continuity.recovery = WorkerRecoveryMode::RequireSandboxContinuity;
        assert_eq!(
            can_assign(&replacement, &continuity, Some(&previous), false, 10),
            Err(AssignmentRejection::SandboxContinuityUnavailable)
        );
        assert!(can_assign(&replacement, &continuity, Some(&previous), true, 10).is_ok());

        let rebuild = requirements();
        assert!(can_assign(&replacement, &rebuild, Some(&previous), false, 10).is_ok());
    }

    proptest! {
        #[test]
        fn accepted_capabilities_are_always_a_subset(
            offered in prop::collection::btree_set("[a-z]{1,5}", 0..12),
            required in prop::collection::btree_set("[a-z]{1,5}", 0..12),
        ) {
            let manifest = WorkerManifest {
                capabilities: offered.clone(),
                ..WorkerManifest::default()
            };
            let requirements = PlacementRequirements {
                required_capabilities: required.clone(),
                ..PlacementRequirements::default()
            };
            if can_claim(&manifest, &requirements).is_ok() {
                prop_assert!(required.is_subset(&offered));
            }
        }

        #[test]
        fn adding_a_missing_requirement_never_preserves_eligibility(
            offered in prop::collection::btree_set("[a-z]{1,5}", 0..12),
            missing in "[A-Z]{1,5}",
        ) {
            let manifest = WorkerManifest {
                capabilities: offered,
                ..WorkerManifest::default()
            };
            let mut requirements = PlacementRequirements::default();
            requirements.required_capabilities.insert(missing);
            prop_assert!(can_claim(&manifest, &requirements).is_err());
        }
    }
}
