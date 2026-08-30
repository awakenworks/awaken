//! Domain-owned operation vocabulary for terminal Session cleanup.
//!
//! The Session aggregate owns whether cleanup is required or complete. Runtime
//! implementations own the substrate-specific effects, but must execute them
//! from a stable [`SessionCleanupCommand`]. Their untrusted completion report
//! becomes a [`VerifiedSessionCleanupReceipt`] only after exact command binding.

mod completion;
mod driver;
mod effects;
mod progress;
mod repository_publication;
mod state;

use completion::SessionCleanupCompletion;
use completion::VerifiedSessionCleanupReceipt;
pub use driver::{SessionTerminalCleanupDriveOutcome, drive_session_terminal_cleanup};
pub use effects::*;
pub use progress::{
    SessionCleanupDisposalCommand, SessionCleanupDisposalReceipt, SessionCleanupPreparation,
    SessionCleanupRepositoryPreparation, SessionTerminalCleanupAction,
};
use progress::{SessionCleanupDisposing, SessionCleanupPreparing};
use repository_publication::{
    SessionRepositoryPublicationCleanup, VerifiedRepositoryPublicationOutcome,
    verified_repository_publication_outcome, verified_repository_publication_outcome_for,
};
pub use repository_publication::{
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection,
};
pub use state::SessionCleanupOperation;
pub(crate) use state::deserialize_persisted_operation;
#[cfg(kani)]
use state::{SessionCleanupPhase, session_cleanup_phase_advance_admitted};

#[cfg(test)]
use effects::cleanup_effect_id;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Heap-free admission kernel for one terminal cleanup effect receipt. The
/// typed boundary performs the exact identity and canonical-fingerprint
/// comparisons; this closed rule makes every required axis explicit and is
/// shared by production verification and exhaustive checking.
#[must_use]
pub(crate) const fn session_cleanup_completion_admitted(
    artifact_effects_unique: bool,
    session_matches: bool,
    thread_matches: bool,
    effect_matches: bool,
    canonical_receipt_matches: bool,
) -> bool {
    artifact_effects_unique
        && session_matches
        && thread_matches
        && effect_matches
        && canonical_receipt_matches
}

#[cfg(test)]
mod tests {
    use super::state::SessionCleanupState;
    use super::*;
    use awaken_provisioning_contract::{
        RepositoryPublicationExpectation, RepositoryPublicationReceipt,
    };
    use awaken_resource_contract::ResourceAccess;
    use proptest::prelude::*;

    fn verified(command: &SessionCleanupCommand) -> VerifiedSessionCleanupReceipt {
        SessionCleanupCompletion::new(command, Vec::new())
            .verify(command)
            .unwrap()
    }

    #[derive(serde::Deserialize)]
    struct PersistedCleanupFixture {
        #[serde(deserialize_with = "super::deserialize_persisted_operation")]
        cleanup: SessionCleanupOperation,
    }

    fn decode_persisted_cleanup(
        value: serde_json::Value,
    ) -> Result<SessionCleanupOperation, serde_json::Error> {
        serde_json::from_value::<PersistedCleanupFixture>(serde_json::json!({
            "cleanup": value,
        }))
        .map(|fixture| fixture.cleanup)
    }

    fn decode_persisted_cleanup_slice(
        encoded: &[u8],
    ) -> Result<SessionCleanupOperation, serde_json::Error> {
        decode_persisted_cleanup(serde_json::from_slice(encoded)?)
    }

    fn decode_persisted_cleanup_str(
        encoded: &str,
    ) -> Result<SessionCleanupOperation, serde_json::Error> {
        decode_persisted_cleanup(serde_json::from_str(encoded)?)
    }

    /// Construct old persisted bytes without retaining a production one-stage
    /// completion writer. Cause/effect decision table: C1 an exact legacy
    /// Requested wire is plain or publication-wrapped; C2 its completion is
    /// canonical or malformed. E1 the private aggregate-field codec admits the
    /// canonical historical bytes; E2 verification rejects malformed evidence;
    /// E3 the opaque public operation exposes no completion mutation or decoder.
    ///
    /// | Rule | wrapper | completion | Effect |
    /// | L1 | absent | canonical | E1/E3 |
    /// | L2 | publication | canonical | E1/E3 |
    /// | L3 | either | malformed | E2/E3 |
    fn install_legacy_completion_wire(
        state: &mut SessionCleanupOperation,
        completion: SessionCleanupCompletion,
    ) {
        let thread_id = completion.thread_id.clone();
        let mut encoded = serde_json::to_value(&*state).unwrap();
        let cleanup = if encoded.get("state").and_then(serde_json::Value::as_str)
            == Some("repository_publication")
        {
            encoded
                .get_mut("cleanup")
                .expect("publication wire contains its cleanup")
        } else {
            &mut encoded
        };
        assert_eq!(
            cleanup.get("state").and_then(serde_json::Value::as_str),
            Some("requested"),
            "legacy completion fixture requires Requested cleanup"
        );
        cleanup
            .as_object_mut()
            .expect("cleanup wire is an object")
            .entry("completions")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("legacy completions wire is an object")
            .insert(thread_id, serde_json::to_value(completion).unwrap());
        *state = decode_persisted_cleanup(encoded).unwrap();
    }

    fn publication_intent() -> SessionRepositoryPublicationIntent {
        SessionRepositoryPublicationIntent {
            input: crate::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::from("source"),
                source: crate::ResolvedInputSource::Repository {
                    repository_id: awaken_resource_contract::RepositoryId::from("repo-1"),
                    config: awaken_resource_contract::RepositoryConfigVersion {
                        repository_id: awaken_resource_contract::RepositoryId::from("repo-1"),
                        version: awaken_resource_contract::ConfigVersion(7),
                        remote_url: "https://example.test/repo.git".into(),
                        credential_binding: None,
                        initial_branch: Some("main".into()),
                        initial_commit: None,
                        clone_policy: Default::default(),
                    },
                    credential: None,
                },
                mount_path: "/workspace/source".into(),
                access: ResourceAccess::ReadWrite,
                instructions: None,
            },
            expectation: RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                expected_prior_commit: None,
            },
        }
    }

    fn memory_input(
        binding_id: &str,
        store_id: &str,
        mount_path: &str,
        access: ResourceAccess,
    ) -> crate::ResolvedInput {
        crate::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from(binding_id),
            source: crate::ResolvedInputSource::MemoryStore {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from(store_id),
                config: awaken_resource_contract::MemoryStoreConfigVersion {
                    memory_store_id: awaken_resource_contract::MemoryStoreId::from(store_id),
                    version: awaken_resource_contract::ConfigVersion(7),
                    retention_policy: Default::default(),
                },
            },
            mount_path: mount_path.into(),
            access,
            instructions: None,
        }
    }

    #[test]
    fn terminal_memory_batch_projection_is_exact_and_fail_closed() {
        // Cause/effect decision table:
        // | Rule | evidence -> frozen mount | source/store | access | Effect |
        // | B1 | exactly one | exact Memory | RW | one existing single-item intent |
        // | B2 | exactly one | exact Memory | RO | explicit skip |
        // | B3 | zero or multiple inputs for evidence | any | any | ResourceMismatch |
        // | B4 | no evidence for frozen input | exact Memory | RW | no intent (FUSE) |
        // | B5 | exactly one | non-Memory or wrong store | any | ResourceMismatch |
        // | B6 | duplicate/noncanonical or malformed evidence | any | any | reject |
        // The batch layer owns only correlation/cardinality. Config, binding,
        // access, materialization and root-effect validation remain in
        // SessionTerminalMemoryIntent::try_new.
        let effect = SessionTerminalCleanupEffect::new(
            SessionCleanupCommand::new("session-memory", "session-memory", "cleanup-root"),
            crate::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker:boot".into(),
                epoch: 3,
                expires_at_unix_ms: u64::MAX,
            },
        );
        let rw = memory_input(
            "binding-rw",
            "store-rw",
            "/memory/rw",
            ResourceAccess::ReadWrite,
        );
        let ro = memory_input(
            "binding-ro",
            "store-ro",
            "/memory/ro",
            ResourceAccess::ReadOnly,
        );
        let ro_evidence = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "store-ro",
            "/memory/ro",
            Vec::new(),
        )
        .unwrap();
        let rw_evidence = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "store-rw",
            "/memory/rw",
            Vec::new(),
        )
        .unwrap();

        let intents = terminal_memory_reconciliation_intents(
            &[rw.clone(), ro.clone()],
            &[ro_evidence.clone(), rw_evidence.clone()],
            &effect,
        )
        .expect("B1/B2 exact projection");
        assert_eq!(intents.len(), 1, "B2 RO skipped");
        assert_eq!(intents[0].binding_id(), &rw.binding_id, "B1 binding");
        assert_eq!(intents[0].memory_store_id(), "store-rw", "B1 store");

        assert!(
            terminal_memory_reconciliation_intents(std::slice::from_ref(&ro), &[], &effect,)
                .expect("B2 RO does not require evidence")
                .is_empty()
        );

        let missing = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "store-missing",
            "/memory/missing",
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            terminal_memory_reconciliation_intents(&[rw.clone(), ro.clone()], &[missing], &effect,),
            Err(SessionMemoryReconciliationError::ResourceMismatch),
            "B3 extra evidence"
        );

        let mut ambiguous = vec![rw.clone(), rw.clone()];
        ambiguous[1].binding_id = awaken_resource_contract::BindingId::from("binding-other");
        assert_eq!(
            terminal_memory_reconciliation_intents(
                &ambiguous,
                std::slice::from_ref(&rw_evidence),
                &effect,
            ),
            Err(SessionMemoryReconciliationError::ResourceMismatch),
            "B3 ambiguous"
        );

        assert!(
            terminal_memory_reconciliation_intents(
                &[rw.clone(), ro.clone()],
                std::slice::from_ref(&ro_evidence),
                &effect,
            )
            .expect("B4 write-through/FUSE has no copy evidence")
            .is_empty(),
            "B4"
        );

        let wrong_store = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "store-other",
            "/memory/rw",
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            terminal_memory_reconciliation_intents(
                std::slice::from_ref(&rw),
                std::slice::from_ref(&wrong_store),
                &effect,
            ),
            Err(SessionMemoryReconciliationError::ResourceMismatch),
            "B5 store"
        );
        let non_memory = crate::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("binding-file"),
            source: crate::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from("file"),
            },
            mount_path: "/memory/rw".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        };
        assert_eq!(
            terminal_memory_reconciliation_intents(
                &[non_memory],
                std::slice::from_ref(&rw_evidence),
                &effect,
            ),
            Err(SessionMemoryReconciliationError::ResourceMismatch),
            "B5 source"
        );

        assert!(
            matches!(
                terminal_memory_reconciliation_intents(
                    &[rw.clone(), ro.clone()],
                    &[rw_evidence.clone(), ro_evidence.clone()],
                    &effect,
                ),
                Err(SessionMemoryReconciliationError::InvalidIntent(_))
            ),
            "B6 noncanonical"
        );
        assert!(
            matches!(
                terminal_memory_reconciliation_intents(
                    std::slice::from_ref(&rw),
                    &[rw_evidence.clone(), rw_evidence.clone()],
                    &effect,
                ),
                Err(SessionMemoryReconciliationError::InvalidIntent(_))
            ),
            "B6 duplicate"
        );

        // Optional-handle terminal rules share the same private presence
        // classifier as continuation: T1 None+RW is legacy ambiguous and fails
        // before Artifact/provider effects; T2 Some([])+RW is explicit
        // WTR/FUSE and returns no intent plus an ack-able empty slice; T3
        // Some(exact RW Copy) returns the existing intent plus the exact complete
        // slice; T4 None+RO has no write-back or acknowledgement.
        assert_eq!(
            terminal_memory_reconciliation_intents_from_materializations(
                std::slice::from_ref(&rw),
                None,
                &effect,
            ),
            Err(SessionMemoryReconciliationError::WritableCopyRequiresTerminalReconciliation),
            "T1"
        );
        assert_eq!(
            terminal_memory_reconciliation_intents_from_materializations(
                std::slice::from_ref(&rw),
                Some(&[]),
                &effect,
            ),
            Ok((Vec::new(), Some([].as_slice()))),
            "T2"
        );
        let (terminal_intents, terminal_evidence) =
            terminal_memory_reconciliation_intents_from_materializations(
                std::slice::from_ref(&rw),
                Some(std::slice::from_ref(&rw_evidence)),
                &effect,
            )
            .expect("T3");
        assert_eq!(terminal_intents.len(), 1, "T3 intent");
        assert_eq!(
            terminal_evidence,
            Some(std::slice::from_ref(&rw_evidence)),
            "T3 evidence"
        );
        assert_eq!(
            terminal_memory_reconciliation_intents_from_materializations(
                std::slice::from_ref(&ro),
                None,
                &effect,
            ),
            Ok((Vec::new(), None)),
            "T4"
        );

        // Continuation reuses the exact same evidence/input join. C1 all joined
        // copy evidence is RO / any item is RW; C2 evidence is exact / foreign.
        // C1-RO+C2-exact => no Memory I/O is required and source release may ack
        // the complete evidence; C1-RW+C2-exact => typed fail-closed result before
        // provider mutation; !C2 => the same ResourceMismatch as terminal join.
        assert_eq!(
            validate_continuation_memory_reconciliation(
                std::slice::from_ref(&ro),
                std::slice::from_ref(&ro_evidence),
            ),
            Ok(()),
            "C1 RO"
        );
        assert_eq!(
            validate_continuation_memory_reconciliation(&[ro.clone(), rw.clone()], &[]),
            Ok(()),
            "C1 FUSE/no copy evidence"
        );
        assert_eq!(
            validate_continuation_memory_reconciliation(
                std::slice::from_ref(&rw),
                std::slice::from_ref(&rw_evidence),
            ),
            Err(SessionMemoryReconciliationError::WritableCopyRequiresTerminalReconciliation),
            "C1 RW copy"
        );
        assert_eq!(
            validate_continuation_memory_reconciliation(
                std::slice::from_ref(&ro),
                std::slice::from_ref(&wrong_store),
            ),
            Err(SessionMemoryReconciliationError::ResourceMismatch),
            "C2 foreign"
        );

        // The handle-level wrapper is the one None/Some authority shared by
        // suspend admission and source disposal. `None` is legacy unknown, not
        // equivalent to the current explicit `Some([])` WTR/FUSE evidence.
        // | Rule | handle evidence | frozen Memory | Effect |
        // | C3 | None | any RW | typed legacy-ambiguous rejection |
        // | C4 | None | RO only or none | Ok(None), no acknowledgement |
        // | C5 | Some([]) | RW | Ok(Some(empty)), acknowledge exact no-Copy |
        // | C6 | Some(exact Copy) | RO | Ok(Some(slice)), acknowledge all |
        // | C7 | Some(exact Copy) | RW | typed continuation rejection |
        // | C8 | Some(foreign/noncanonical) | any | exact join rejection |
        assert_eq!(
            validate_continuation_memory_materializations(std::slice::from_ref(&rw), None,),
            Err(SessionMemoryReconciliationError::WritableCopyRequiresTerminalReconciliation),
            "C3"
        );
        assert_eq!(
            validate_continuation_memory_materializations(std::slice::from_ref(&ro), None),
            Ok(None),
            "C4 RO"
        );
        assert_eq!(
            validate_continuation_memory_materializations(&[], None),
            Ok(None),
            "C4 no Memory"
        );
        assert_eq!(
            validate_continuation_memory_materializations(std::slice::from_ref(&rw), Some(&[]),),
            Ok(Some([].as_slice())),
            "C5"
        );
        assert_eq!(
            validate_continuation_memory_materializations(
                std::slice::from_ref(&ro),
                Some(std::slice::from_ref(&ro_evidence)),
            ),
            Ok(Some(std::slice::from_ref(&ro_evidence))),
            "C6"
        );
        assert_eq!(
            validate_continuation_memory_materializations(
                std::slice::from_ref(&rw),
                Some(std::slice::from_ref(&rw_evidence)),
            ),
            Err(SessionMemoryReconciliationError::WritableCopyRequiresTerminalReconciliation),
            "C7"
        );
        assert_eq!(
            validate_continuation_memory_materializations(
                std::slice::from_ref(&ro),
                Some(std::slice::from_ref(&wrong_store)),
            ),
            Err(SessionMemoryReconciliationError::ResourceMismatch),
            "C8"
        );
    }

    fn publication_receipt(
        command: &SessionRepositoryPublicationCommand,
    ) -> SessionRepositoryPublicationReceipt {
        let (repository_id, remote_url) = command.intent.repository_target().unwrap();
        SessionRepositoryPublicationReceipt::new(
            command,
            RepositoryPublicationReceipt {
                repository_id: repository_id.to_string(),
                source_remote_url: remote_url.to_string(),
                branch: command.intent.expectation.branch.clone(),
                commit: command.intent.expectation.commit.clone(),
            },
        )
    }

    #[allow(dead_code)]
    enum LegacySessionCleanupOperationLayout {
        NotRequested,
        Fenced {
            effect_id: String,
        },
        Requested {
            effect_id: String,
            thread_ids: BTreeSet<String>,
            delegation_watermark: u64,
            runtime_commit_cursor: Option<u64>,
            completions: BTreeMap<String, SessionCleanupCompletion>,
        },
        Completed {
            effect_id: String,
            thread_ids: BTreeSet<String>,
            delegation_watermark: u64,
            runtime_commit_cursor: Option<u64>,
            receipt_fingerprint: String,
        },
    }

    #[test]
    fn opaque_cleanup_keeps_the_private_legacy_wire_layout_exact() {
        // Layout cause/effect decision table: C1 PersistedSession embeds the
        // opaque cleanup operation by value; C2 the private wire state retains
        // the four historical field sets; C3 publication is absent/present.
        // Effects: E1 the wrapper retains the legacy size/alignment; E2 no
        // public Rust variant can construct or destructure completion evidence;
        // E3 publication contributes only one boxed-wrapper word; E4 serde emits
        // the exact historical tagged payload without a newtype layer.
        //
        // | Rule | publication | legacy fields | Effect |
        // | B1 | absent | private/exact | E1/E2 |
        // | B2 | present | isolated in wrapper | E1/E3/E4 |
        //
        // On x86_64 the uncorrected publication layout measured 120 bytes for
        // SessionCleanupOperation. Boxing the one additive wrapper preserves
        // the private state's exact 104-byte layout. The whole
        // PersistedSession size is deliberately not duplicated here: unrelated
        // aggregate fields have their own layout evolution, while equal cleanup
        // size and alignment prove this sidecar adds no inline bytes.
        assert_eq!(
            std::mem::size_of::<SessionCleanupOperation>(),
            std::mem::size_of::<LegacySessionCleanupOperationLayout>(),
            "B1-B2/E1 publication must not enlarge the legacy cleanup layout"
        );
        assert_eq!(
            std::mem::align_of::<SessionCleanupOperation>(),
            std::mem::align_of::<LegacySessionCleanupOperationLayout>(),
            "B1-B2/E1 publication must not change legacy cleanup alignment"
        );
        assert!(
            std::mem::size_of::<SessionCleanupOperation>()
                < std::mem::size_of::<SessionRepositoryPublicationReceipt>(),
            "B1-B2/E3 cleanup must not inline publication payloads"
        );
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<SessionCleanupOperation>(), 104, "E1");
        }
        let legacy_requested =
            SessionCleanupOperation::from_state(SessionCleanupState::Requested {
                effect_id: String::new(),
                thread_ids: BTreeSet::new(),
                delegation_watermark: 0,
                runtime_commit_cursor: None,
                completions: BTreeMap::new(),
            });
        let legacy_completed =
            SessionCleanupOperation::from_state(SessionCleanupState::Completed {
                effect_id: String::new(),
                thread_ids: BTreeSet::new(),
                delegation_watermark: 0,
                runtime_commit_cursor: None,
                receipt_fingerprint: String::new(),
            });
        assert_eq!(
            serde_json::to_value(legacy_requested).unwrap()["state"],
            "requested",
            "B1/E4"
        );
        assert_eq!(
            serde_json::to_value(legacy_completed).unwrap()["state"],
            "completed",
            "B1/E4"
        );

        let intent = publication_intent();
        let mut state = SessionCleanupOperation::default();
        state
            .request_with_publication("boxed-wire", intent.clone())
            .unwrap();
        let encoded = serde_json::to_value(&state).unwrap();
        assert_eq!(
            encoded.get("intent"),
            Some(&serde_json::to_value(intent).unwrap()),
            "B2/E4"
        );
    }

    #[test]
    fn publication_wrapper_deserialization_rejects_recursive_or_impossible_state() {
        // Decoder cause/effect decision table: C1 the additive wrapper contains
        // exactly one legacy cleanup; C2 that inner cleanup is itself a wrapper;
        // C3 the inner cleanup has not been requested; C4 a completed inner
        // cleanup has no publication receipt. E1 admits the one-level shape;
        // E2 rejects before a recursive or phase-inconsistent authority enters
        // PersistedSession.
        //
        // | Rule | inner cleanup | receipt shape | Effect |
        // | D1 | Fenced legacy | absent | E1 |
        // | D2 | publication wrapper | absent | E2 nested |
        // | D3 | NotRequested | absent | E2 unrequested |
        // | D4 | Completed legacy | absent | E2 missing receipt |
        let intent = publication_intent();
        let mut state = SessionCleanupOperation::default();
        state
            .request_with_publication("decode-shape", intent.clone())
            .unwrap();
        let valid = serde_json::to_value(&state).unwrap();
        assert!(decode_persisted_cleanup(valid.clone()).is_ok(), "D1/E1");

        let nested = serde_json::json!({
            "state": "repository_publication",
            "cleanup": valid,
            "intent": intent,
        });
        let nested_error = decode_persisted_cleanup(nested).expect_err("D2/E2");
        assert!(
            nested_error
                .to_string()
                .contains("nested Repository publication cleanup is forbidden"),
            "D2/E2: {nested_error}"
        );

        let unrequested = serde_json::json!({
            "state": "repository_publication",
            "cleanup": { "state": "not_requested" },
            "intent": publication_intent(),
        });
        assert!(decode_persisted_cleanup(unrequested).is_err(), "D3/E2");

        let completed_without_receipt = serde_json::json!({
            "state": "repository_publication",
            "cleanup": {
                "state": "completed",
                "effect_id": cleanup_effect_id("decode-shape"),
                "thread_ids": ["decode-shape"],
                "delegation_watermark": 0,
                "runtime_commit_cursor": 0,
                "receipt_fingerprint": "fnv1a64:0000000000000000"
            },
            "intent": publication_intent(),
        });
        assert!(
            decode_persisted_cleanup(completed_without_receipt).is_err(),
            "D4/E2"
        );
    }

    #[test]
    fn intent_and_receipt_are_stable_across_recovery() {
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("session-1"));
        assert!(state.is_fenced());
        assert!(state.freeze_targets("session-1", [], 7, 13).unwrap());
        let first = state.command_for("session-1", "session-1").unwrap();
        let replay = state.command_for("session-1", "session-1").unwrap();
        assert_eq!(first, replay);
        let receipt = verified(&first);
        assert!(state.complete("session-1", &[receipt]).unwrap());
        assert!(state.is_completed());
        assert_eq!(
            state.runtime_commit_cursor(),
            Some(13),
            "the quiescent Runtime high-water survives Requested -> Completed"
        );
    }

    #[test]
    fn legacy_no_publication_wire_and_fingerprints_remain_exact_v1() {
        // Compatibility cause/effect decision table: C1 an existing caller uses
        // `request`; C2 no Repository publication fields exist. Effects: E1 the
        // Fenced/Requested/Completed JSON bytes remain exact; E2 the root effect,
        // thread command, thread completion, and v1 aggregate receipt identities
        // retain their pre-publication values; E3 legacy JSON decodes without
        // inventing an intent.
        //
        // | Rule | request API | publication fields | phase | Effect |
        // | L1 | legacy | absent | Fenced | E1/E2 |
        // | L2 | legacy | absent | Requested | E1/E2/E3 |
        // | L3 | legacy | absent | Completed | E1/E2/E3 |
        //
        // These constants were produced by the authoritative v1 implementation
        // before optional Repository publication fields were introduced.
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("legacy-session"));
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"state":"fenced","effect_id":"fnv1a64:0f89047907a93a47"}"#,
            "L1/E1"
        );

        assert!(state.freeze_targets("legacy-session", [], 7, 11).unwrap());
        let requested = r#"{"state":"requested","effect_id":"fnv1a64:0f89047907a93a47","thread_ids":["legacy-session"],"delegation_watermark":7,"runtime_commit_cursor":11}"#;
        assert_eq!(serde_json::to_string(&state).unwrap(), requested, "L2/E1");
        let decoded = decode_persisted_cleanup_str(requested).unwrap();
        assert!(decoded.repository_publication_intent().is_none(), "L2/E3");

        let command = state
            .command_for("legacy-session", "legacy-session")
            .unwrap();
        assert_eq!(command.effect_id, "fnv1a64:34c458e86e3a9559", "L2/E2");
        let completion = SessionCleanupCompletion::new(&command, Vec::new());
        assert_eq!(
            completion.receipt_fingerprint, "fnv1a64:d5eb131b9e72fe50",
            "L2/E2"
        );
        assert!(
            state
                .complete("legacy-session", &[completion.verify(&command).unwrap()])
                .unwrap()
        );
        let completed = r#"{"state":"completed","effect_id":"fnv1a64:0f89047907a93a47","thread_ids":["legacy-session"],"delegation_watermark":7,"runtime_commit_cursor":11,"receipt_fingerprint":"fnv1a64:b5e378ea35d2300b"}"#;
        assert_eq!(serde_json::to_string(&state).unwrap(), completed, "L3/E1");
        let decoded = decode_persisted_cleanup_str(completed).unwrap();
        assert!(decoded.repository_publication_intent().is_none(), "L3/E3");
        assert!(
            decoded
                .repository_publication_receipt("legacy-session")
                .unwrap()
                .is_none(),
            "L3/E3"
        );
    }

    #[test]
    fn repository_publication_is_child_first_durable_and_root_gated() {
        // Cause/effect graph: C1 an explicit valid publication intent is frozen;
        // C2 child cleanup is pending/complete; C3 publication receipt is
        // absent/present; C4 root cleanup is asserted; C5 process recovery may
        // occur after either durable effect. Effects: E1 children are the only
        // first commands; E2 publication becomes the sole root projection; E3
        // ordinary root disposal is withheld until publication evidence; E4
        // exact receipts replay without a second effect; E5 recovery preserves
        // the same command/receipt and reaches one v2 terminal outcome.
        //
        // | Rule | child | publication receipt | root cleanup | restart | Effect |
        // | R1 | pending | absent | no | no | E1 |
        // | R2 | complete | absent | no | yes | E2/E3/E5 |
        // | R3 | complete | absent | asserted | no | reject E3 |
        // | R4 | complete | exact new/replay | no | yes | E4/E5 |
        // | R5 | complete | present | asserted | no | admit root |
        // | R6 | complete | present | complete | yes | one v2 terminal fact |
        let mut state = SessionCleanupOperation::default();
        let intent = publication_intent();
        assert!(
            state
                .request_with_publication("publish-session", intent.clone())
                .unwrap()
        );
        assert!(
            !state
                .request_with_publication("publish-session", intent)
                .unwrap(),
            "R1 exact request replay"
        );
        state
            .freeze_targets("publish-session", ["publish-child".to_string()], 17, 23)
            .unwrap();
        let child = state
            .command_for("publish-session", "publish-child")
            .unwrap();
        let root = state
            .command_for("publish-session", "publish-session")
            .unwrap();
        assert_eq!(
            state
                .pending_commands("publish-session")
                .unwrap()
                .iter()
                .map(|command| command.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["publish-child"],
            "R1/E1"
        );
        assert!(
            state
                .publication_command("publish-session")
                .unwrap()
                .is_none(),
            "R1/E1"
        );
        let early_command = SessionRepositoryPublicationCommand::new(
            "publish-session",
            &cleanup_effect_id("publish-session"),
            state.repository_publication_intent().unwrap(),
        )
        .unwrap();
        assert_eq!(
            state.record_repository_publication_receipt(
                "publish-session",
                publication_receipt(&early_command),
            ),
            Err(SessionCleanupError::RepositoryPublicationNotReady),
            "R1 children cannot be bypassed"
        );

        install_legacy_completion_wire(
            &mut state,
            SessionCleanupCompletion::new(&child, Vec::new()),
        );
        assert!(
            state
                .pending_commands("publish-session")
                .unwrap()
                .is_empty(),
            "R2/E3"
        );
        let publication = state
            .publication_command("publish-session")
            .unwrap()
            .expect("R2/E2");
        assert_eq!(publication.session_id, "publish-session", "R2 root-only");
        let mut premature = state.clone();
        install_legacy_completion_wire(
            &mut premature,
            SessionCleanupCompletion::new(&root, Vec::new()),
        );
        let premature_receipts = premature.recorded_receipts("publish-session").unwrap();
        assert_eq!(
            premature.complete("publish-session", &premature_receipts),
            Err(SessionCleanupError::MissingRepositoryPublicationOutcome),
            "R3/E3 historical wire cannot bypass publication"
        );

        let encoded = serde_json::to_vec(&state).unwrap();
        let mut recovered = decode_persisted_cleanup_slice(&encoded).unwrap();
        assert_eq!(
            recovered.publication_command("publish-session").unwrap(),
            Some(publication.clone()),
            "R2/E5"
        );
        let receipt = publication_receipt(&publication);
        assert!(
            recovered
                .record_repository_publication_receipt("publish-session", receipt.clone())
                .unwrap(),
            "R4/E4"
        );
        assert!(
            !recovered
                .record_repository_publication_receipt("publish-session", receipt.clone())
                .unwrap(),
            "R4/E4 exact replay"
        );

        let encoded = serde_json::to_vec(&recovered).unwrap();
        let mut recovered = decode_persisted_cleanup_slice(&encoded).unwrap();
        assert_eq!(
            recovered
                .pending_commands("publish-session")
                .unwrap()
                .iter()
                .map(|command| command.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["publish-session"],
            "R5/E5"
        );
        assert!(
            recovered
                .publication_command("publish-session")
                .unwrap()
                .is_none(),
            "R5"
        );
        install_legacy_completion_wire(
            &mut recovered,
            SessionCleanupCompletion::new(&root, Vec::new()),
        );
        let receipts = recovered.recorded_receipts("publish-session").unwrap();
        assert!(
            recovered.complete("publish-session", &receipts).unwrap(),
            "R6"
        );
        assert!(
            recovered
                .repository_publication_receipt("publish-session")
                .unwrap()
                .is_some(),
            "R6"
        );
        let SessionCleanupState::RepositoryPublication(publication) = recovered.state() else {
            panic!("R6 publication wrapper");
        };
        let SessionCleanupState::Completed {
            receipt_fingerprint,
            ..
        } = publication.cleanup.state()
        else {
            panic!("R6 completed");
        };
        let cleanup_evidence = receipts
            .iter()
            .map(|cleanup| {
                (
                    cleanup.thread_id(),
                    cleanup.effect_id(),
                    cleanup.receipt_fingerprint(),
                )
            })
            .collect::<Vec<_>>();
        let would_be_v1 = crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v1",
            cleanup_effect_id("publish-session"),
            17_u64,
            cleanup_evidence.clone(),
        ));
        let expected_v2 = crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v2",
            cleanup_effect_id("publish-session"),
            17_u64,
            cleanup_evidence,
            receipt.receipt_fingerprint.as_str(),
        ));
        assert_eq!(receipt_fingerprint.as_str(), expected_v2, "R6 v2");
        assert_eq!(
            receipt_fingerprint, "fnv1a64:98d50a5f30ce7e68",
            "R6 exact v2"
        );
        assert_ne!(receipt_fingerprint.as_str(), would_be_v1, "R6 not v1");
        let encoded = serde_json::to_vec(&recovered).unwrap();
        let mut recovered = decode_persisted_cleanup_slice(&encoded).unwrap();
        assert_eq!(
            serde_json::to_string(&recovered).unwrap(),
            r#"{"state":"repository_publication","cleanup":{"state":"completed","effect_id":"fnv1a64:0644e194c63acc8b","thread_ids":["publish-child","publish-session"],"delegation_watermark":17,"runtime_commit_cursor":23,"receipt_fingerprint":"fnv1a64:98d50a5f30ce7e68"},"intent":{"input":{"binding_id":"source","source":{"kind":"repository","repository_id":"repo-1","config":{"repository_id":"repo-1","version":7,"remote_url":"https://example.test/repo.git","initial_branch":"main","clone_policy":{}}},"mount_path":"/workspace/source","access":"read_write"},"expectation":{"branch":"awf/work","commit":"0123456789abcdef0123456789abcdef01234567"}},"receipt":{"command_fingerprint":"fnv1a64:e390852b7b991ab6","effect_receipt":{"repository_id":"repo-1","source_remote_url":"https://example.test/repo.git","branch":"awf/work","commit":"0123456789abcdef0123456789abcdef01234567"},"receipt_fingerprint":"fnv1a64:2350653aaf1ff920"}}"#,
            "R6 exact v2 JSON"
        );
        assert!(
            !recovered
                .record_repository_publication_receipt("publish-session", receipt)
                .unwrap(),
            "R6 exact completed replay"
        );
    }

    #[test]
    fn repository_publication_intent_and_receipt_fail_closed_on_every_axis() {
        // Cause/effect decision table: C1 input kind is Repository; C2 source is
        // writable; C3 source/config identities match; C4 branch is non-empty;
        // C5 commit is full 40-hex; C6 command fingerprint and provisioning
        // receipt coordinates are exact. E1 admits one intent/receipt; E2 rejects
        // before durable mutation. Each invalid rule changes one cause only.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
        // | V1 | T | T | T | T | T | T | E1 |
        // | V2 | F | - | - | - | - | - | E2 |
        // | V3 | T | F | - | - | - | - | E2 |
        // | V4 | T | T | F | - | - | - | E2 |
        // | V5 | T | T | T | F | - | - | E2 |
        // | V6 | T | T | T | T | F | - | E2 |
        // | V7 | T | T | T | T | T | F | E2 |
        let mut invalid_values = Vec::new();
        let mut file = publication_intent();
        file.input.source = crate::ResolvedInputSource::File {
            file_id: awaken_resource_contract::FileId::from("file-1"),
        };
        invalid_values.push(file);
        let mut readonly = publication_intent();
        readonly.input.access = ResourceAccess::ReadOnly;
        invalid_values.push(readonly);
        let mut mismatched_config = publication_intent();
        let crate::ResolvedInputSource::Repository { config, .. } =
            &mut mismatched_config.input.source
        else {
            unreachable!();
        };
        config.repository_id = awaken_resource_contract::RepositoryId::from("repo-2");
        invalid_values.push(mismatched_config);
        let mut blank_branch = publication_intent();
        blank_branch.expectation.branch.clear();
        invalid_values.push(blank_branch);
        let mut short_commit = publication_intent();
        short_commit.expectation.commit = "abc".into();
        invalid_values.push(short_commit);
        let mut non_hex_commit = publication_intent();
        non_hex_commit.expectation.commit = "z".repeat(40);
        invalid_values.push(non_hex_commit);

        for (index, invalid) in invalid_values.into_iter().enumerate() {
            let mut state = SessionCleanupOperation::default();
            assert!(
                matches!(
                    state.request_with_publication("invalid", invalid),
                    Err(SessionCleanupError::InvalidRepositoryPublicationIntent(_))
                ),
                "V{}",
                index + 2
            );
            assert!(state.is_not_requested(), "E2");
        }

        let mut state = SessionCleanupOperation::default();
        let intent = publication_intent();
        state
            .request_with_publication("exact", intent.clone())
            .unwrap();
        let mut changed_intent = intent;
        changed_intent.expectation.commit = "1123456789abcdef0123456789abcdef01234567".into();
        assert_eq!(
            state.request_with_publication("exact", changed_intent),
            Err(SessionCleanupError::FrozenRepositoryPublicationMismatch),
            "frozen intent cannot change"
        );
        state.freeze_targets("exact", [], 0, 0).unwrap();
        let command = state
            .publication_command("exact")
            .unwrap()
            .expect("V1 command");
        let exact = publication_receipt(&command);

        let mut mismatches = Vec::new();
        let mut command_fingerprint = exact.clone();
        command_fingerprint.command_fingerprint.push_str("-stale");
        mismatches.push(command_fingerprint);
        for axis in 0..4 {
            let mut effect = exact.effect_receipt.clone();
            match axis {
                0 => effect.repository_id.push_str("-stale"),
                1 => effect.source_remote_url.push_str("-stale"),
                2 => effect.branch.push_str("-stale"),
                3 => effect.commit.replace_range(..1, "f"),
                _ => unreachable!(),
            }
            mismatches.push(SessionRepositoryPublicationReceipt::new(&command, effect));
        }
        let mut receipt_fingerprint = exact.clone();
        receipt_fingerprint.receipt_fingerprint.push_str("-stale");
        mismatches.push(receipt_fingerprint);

        for mismatch in mismatches {
            assert_eq!(
                state.record_repository_publication_receipt("exact", mismatch),
                Err(SessionCleanupError::RepositoryPublicationReceiptMismatch),
                "V7/E2"
            );
            assert!(
                state
                    .repository_publication_receipt("exact")
                    .unwrap()
                    .is_none(),
                "V7/E2"
            );
        }
        assert!(
            state
                .record_repository_publication_receipt("exact", exact)
                .unwrap(),
            "V1/E1"
        );
    }

    #[test]
    fn legacy_cleanup_rows_do_not_invent_a_terminal_projection_anchor() {
        // Cause/effect decision table: C1 a legacy Requested/Completed row has
        // no Runtime cursor field; C2 a fresh never-run Session durably records
        // cursor zero. E1 legacy decode returns None so ParentTerminal remains
        // withheld; E2 fresh zero remains Some(0) across completion.
        //
        // | Rule | shape | phase | Effect |
        // | L1 | missing cursor | Requested/Completed | E1 |
        // | L2 | explicit zero | Requested/Completed | E2 |
        //
        // The distinction is required because zero is a valid Runtime
        // high-water, not a migration sentinel.
        let mut requested = SessionCleanupOperation::default();
        assert!(requested.request("never-run"));
        assert!(requested.freeze_targets("never-run", [], 0, 0).unwrap());
        assert_eq!(requested.runtime_commit_cursor(), Some(0), "L2 Requested");

        let mut legacy_requested = serde_json::to_value(&requested).unwrap();
        legacy_requested
            .as_object_mut()
            .unwrap()
            .remove("runtime_commit_cursor");
        let legacy_requested = decode_persisted_cleanup(legacy_requested).unwrap();
        assert_eq!(
            legacy_requested.runtime_commit_cursor(),
            None,
            "L1 Requested"
        );

        let root = requested.command_for("never-run", "never-run").unwrap();
        assert!(requested.complete("never-run", &[verified(&root)]).unwrap());
        assert_eq!(requested.runtime_commit_cursor(), Some(0), "L2 Completed");
        let mut legacy_completed = serde_json::to_value(&requested).unwrap();
        legacy_completed
            .as_object_mut()
            .unwrap()
            .remove("runtime_commit_cursor");
        let legacy_completed = decode_persisted_cleanup(legacy_completed).unwrap();
        assert_eq!(
            legacy_completed.runtime_commit_cursor(),
            None,
            "L1 Completed"
        );
    }

    #[test]
    fn child_threads_are_part_of_the_durable_intent_and_required_evidence() {
        let mut state = SessionCleanupOperation::default();
        state.request("session-1");
        assert!(
            state
                .freeze_targets("session-1", ["child-1".to_string()], 11, 13)
                .unwrap()
        );
        let root = state.command_for("session-1", "session-1").unwrap();
        let child = state.command_for("session-1", "child-1").unwrap();
        assert_eq!(
            state.complete("session-1", &[verified(&root)]),
            Err(SessionCleanupError::MissingReceipt)
        );
        assert!(
            state
                .complete("session-1", &[verified(&root), verified(&child),],)
                .unwrap()
        );
    }

    #[test]
    fn mismatched_or_incomplete_receipts_fail_closed() {
        let mut state = SessionCleanupOperation::default();
        state.request("session-1");
        state.freeze_targets("session-1", [], 0, 0).unwrap();
        let command = state.command_for("session-1", "session-1").unwrap();
        let mut completion = SessionCleanupCompletion::new(&command, Vec::new());
        completion.effect_id.push_str("-stale");
        assert_eq!(
            completion.verify(&command),
            Err(SessionCleanupError::ReceiptMismatch)
        );
        assert!(state.is_requested());
    }

    #[test]
    fn frozen_targets_cannot_be_expanded_by_a_late_projection() {
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("session-1"));
        assert!(
            state
                .freeze_targets("session-1", ["child-1".to_string()], 19, 0)
                .unwrap()
        );
        assert!(
            !state
                .freeze_targets("session-1", ["child-1".to_string()], 19, 0)
                .unwrap()
        );
        assert_eq!(
            state.freeze_targets(
                "session-1",
                ["child-1".to_string(), "child-2".to_string()],
                20,
                0,
            ),
            Err(SessionCleanupError::FrozenTargetsMismatch)
        );
    }

    #[test]
    fn cleanup_identity_and_exact_receipt_set_follow_the_decision_table() {
        // Cause/effect graph: C1 the asserted Session matches the fenced root;
        // C2 evidence includes the root exactly once; C3 every frozen child is
        // present exactly once. Effects are E1 freeze/complete, or E2 a precise
        // fail-closed error without changing durable Requested state.
        //
        // | Rule | Session | root receipt | duplicate | child set | Effect |
        // | T05 | foreign | n/a | no | exact | OperationMismatch |
        // | T06 | exact | missing | no | child only | MissingRootReceipt |
        // | T07 | exact | present | yes | exact | DuplicateThread |
        // | T04 | exact | present | no | expanded after freeze | FrozenTargetsMismatch |
        let mut foreign = SessionCleanupOperation::default();
        foreign.request("session-a");
        assert_eq!(
            foreign.freeze_targets("session-b", [], 1, 0),
            Err(SessionCleanupError::OperationMismatch),
            "T05"
        );
        assert!(foreign.is_fenced(), "T05 leaves Session-A unchanged");

        let mut state = SessionCleanupOperation::default();
        state.request("session-a");
        state
            .freeze_targets("session-a", ["child-a".to_string()], 7, 0)
            .unwrap();
        let root = state.command_for("session-a", "session-a").unwrap();
        let child = state.command_for("session-a", "child-a").unwrap();
        let root_receipt = verified(&root);
        let child_receipt = verified(&child);
        assert_eq!(
            state.complete("session-a", std::slice::from_ref(&child_receipt)),
            Err(SessionCleanupError::MissingRootReceipt),
            "T06"
        );
        assert!(state.is_requested(), "T06");
        assert_eq!(
            state.complete(
                "session-a",
                &[root_receipt.clone(), root_receipt, child_receipt],
            ),
            Err(SessionCleanupError::DuplicateThread(
                "session-a".to_string()
            )),
            "T07"
        );
        assert!(state.is_requested(), "T07");
    }

    #[test]
    fn receipt_order_and_watermark_follow_the_decision_table() {
        // Cause/effect graph: C1 receipt arrival order varies; C2 the frozen
        // watermark is replayed exactly. Effects: E1 order is canonical; E2 a
        // different watermark cannot rewrite frozen truth. Adapter completion is
        // represented by returning this receipt, not by self-asserted booleans.
        //
        // | Rule | order | watermark | Effect |
        // | T09 | root/child or child/root | exact | same fingerprint |
        // | T10 | any | changed | FrozenTargetsMismatch |

        fn completed_with_order(reverse: bool) -> SessionCleanupOperation {
            let mut state = SessionCleanupOperation::default();
            state.request("session-order");
            state
                .freeze_targets("session-order", ["child-order".to_string()], 9, 0)
                .unwrap();
            let root = state.command_for("session-order", "session-order").unwrap();
            let child = state.command_for("session-order", "child-order").unwrap();
            let mut receipts = vec![verified(&root), verified(&child)];
            if reverse {
                receipts.reverse();
            }
            state.complete("session-order", &receipts).unwrap();
            state
        }
        let forward = completed_with_order(false);
        let reverse = completed_with_order(true);
        assert_eq!(forward, reverse, "T09 canonical receipt order");

        let mut watermark = SessionCleanupOperation::default();
        watermark.request("session-watermark");
        watermark
            .freeze_targets("session-watermark", ["child".to_string()], 10, 0)
            .unwrap();
        assert_eq!(
            watermark.freeze_targets("session-watermark", ["child".to_string()], 11, 0),
            Err(SessionCleanupError::FrozenTargetsMismatch),
            "T10"
        );
    }

    #[test]
    fn historical_completion_wire_is_read_only_exact_and_replay_safe() {
        // Cause/effect graph: C1 an old Requested wire already contains one
        // canonical completion; C2 the same bytes decode again; C3 a historical
        // full row contains a conflicting completion; C4 cold recovery contains the
        // full canonical set. Effects: E1 read projection removes only the
        // evidenced target; E2 re-decode is identical and authors nothing; E3
        // verification fails closed; E4 the legacy aggregate can finish its
        // pre-existing completion transition without exposing a write API; E5
        // Completed reprojects commands only for process-local acknowledgement.
        //
        // | Rule | persisted wire | restart | Effect |
        // | R1 | one canonical child completion | no | E1 |
        // | R2 | exact same bytes | yes | E2 |
        // | R3 | full set, conflicting child fingerprint | yes | E3 |
        // | R4 | canonical child + root completions | yes | E4 |
        // | R5 | Completed after R4 | yes | E5 |
        // Constraints/invariants: the frozen target set and canonical receipt
        // identity never change across recovery, and no production port can
        // author another one-stage completion.
        let mut state = SessionCleanupOperation::default();
        state.request("remote-session");
        state
            .freeze_targets("remote-session", ["remote-child".to_string()], 17, 0)
            .unwrap();
        let child = state.command_for("remote-session", "remote-child").unwrap();
        let child_completion = SessionCleanupCompletion::new(&child, Vec::new());
        install_legacy_completion_wire(&mut state, child_completion.clone());
        assert_eq!(state.pending_commands("remote-session").unwrap().len(), 1);
        assert_eq!(
            state.has_complete_legacy_receipts("remote-session"),
            Ok(false),
            "R1/E1 partial historical evidence is read-only, not normalizable"
        );

        let encoded = serde_json::to_vec(&state).unwrap();
        let replayed = decode_persisted_cleanup_slice(&encoded).unwrap();
        assert_eq!(replayed, state, "R2/E2");
        let mut conflicting = child_completion;
        conflicting.receipt_fingerprint.push_str("-stale");
        let mut conflicting_wire = state.clone();
        let conflicting_root = conflicting_wire
            .command_for("remote-session", "remote-session")
            .unwrap();
        install_legacy_completion_wire(
            &mut conflicting_wire,
            SessionCleanupCompletion::new(&conflicting_root, Vec::new()),
        );
        install_legacy_completion_wire(&mut conflicting_wire, conflicting);
        assert_eq!(
            conflicting_wire.has_complete_legacy_receipts("remote-session"),
            Err(SessionCleanupError::ReceiptMismatch),
            "R3/E3"
        );

        let mut recovered = decode_persisted_cleanup_slice(&encoded).unwrap();
        let root = recovered
            .command_for("remote-session", "remote-session")
            .unwrap();
        install_legacy_completion_wire(
            &mut recovered,
            SessionCleanupCompletion::new(&root, Vec::new()),
        );
        assert!(
            recovered
                .pending_commands("remote-session")
                .unwrap()
                .is_empty(),
            "R4/E4"
        );
        assert_eq!(
            recovered.has_complete_legacy_receipts("remote-session"),
            Ok(true),
            "R4/E4 only the complete canonical persisted set is normalizable"
        );
        assert!(
            recovered
                .normalize_legacy_completion("remote-session")
                .unwrap()
        );
        assert!(
            recovered
                .pending_commands("remote-session")
                .unwrap()
                .is_empty(),
            "R5/E5 Completed never recreates provider work"
        );
        assert_eq!(
            recovered.command_for("remote-session", "remote-child"),
            Some(child),
            "R5/E5 child acknowledgement uses the frozen target"
        );
        assert_eq!(
            recovered.command_for("remote-session", "remote-session"),
            Some(root),
            "R5/E5 root acknowledgement uses the frozen target"
        );
    }

    #[test]
    fn every_terminal_cleanup_receipt_identity_axis_is_mandatory() {
        for missing in 0..5 {
            let mut axes = [true; 5];
            axes[missing] = false;
            assert!(
                !session_cleanup_completion_admitted(axes[0], axes[1], axes[2], axes[3], axes[4],),
                "receipt axis {missing}"
            );
        }
        assert!(session_cleanup_completion_admitted(
            true, true, true, true, true
        ));
    }

    proptest! {
        #[test]
        fn random_cleanup_command_sequences_refine_the_monotonic_state_model(
            actions in proptest::collection::vec(0_u8..6, 0..80),
            watermark in any::<u64>(),
        ) {
            /* Model-based cause/effect design. Each generated action is one of:
             * C0 request, C1 exact freeze, C2 exact receipt settlement, C3
             * foreign freeze, C4 request replay, C5 alternate freeze (valid as
             * the first freeze, mismatched once another target is frozen).
             * Effects/invariants: E1 state rank never decreases; E2 Completed is
             * immutable; E3 a frozen command exists in Requested and remains
             * available in Completed solely for final-CAS acknowledgement,
             * while Completed has no pending provider work; E4 foreign or
             * already-frozen mismatched commands never rewrite authority.
             * Random sequences cover order/replay combinations after the
             * deterministic decision table owns each individual oracle. */
            let mut state = SessionCleanupOperation::default();
            for action in actions {
                let before = state.clone();
                let before_rank = cleanup_rank(&before);
                match action {
                    0 | 4 => {
                        state.request("model-session");
                    }
                    1 => {
                        let _ = state.freeze_targets(
                            "model-session",
                            ["model-child".to_string()],
                            watermark,
                            0,
                        );
                    }
                    2 => {
                        if state.is_requested() {
                            let receipts = state
                                .thread_ids()
                                .unwrap()
                                .iter()
                                .map(|thread_id| {
                                    let command = state
                                        .command_for("model-session", thread_id)
                                        .unwrap();
                                    SessionCleanupCompletion::new(&command, Vec::new())
                                    .verify(&command)
                                    .unwrap()
                                })
                                .collect::<Vec<_>>();
                            state.complete("model-session", &receipts).unwrap();
                        } else if state.is_completed() {
                            prop_assert_eq!(state.complete("model-session", &[]), Ok(false));
                        } else {
                            prop_assert_eq!(
                                state.complete("model-session", &[]),
                                Err(SessionCleanupError::NotRequested),
                            );
                        }
                    }
                    3 => {
                        let _ = state.freeze_targets("foreign-session", [], watermark, 0);
                    }
                    5 => {
                        let _ = state.freeze_targets(
                            "model-session",
                            ["late-child".to_string()],
                            watermark.wrapping_add(1),
                            0,
                        );
                    }
                    _ => unreachable!(),
                }
                prop_assert!(cleanup_rank(&state) >= before_rank, "E1");
                if before.is_completed() {
                    prop_assert_eq!(&state, &before, "E2");
                }
                prop_assert_eq!(
                    state.command_for("model-session", "model-session").is_some(),
                    state.is_requested() || state.is_completed(),
                    "E3 frozen command projection",
                );
                if state.is_completed() {
                    prop_assert!(
                        state.pending_commands("model-session").unwrap().is_empty(),
                        "E3 Completed has no provider work",
                    );
                }
                if action == 3 || (action == 5 && (before.is_requested() || before.is_completed())) {
                    prop_assert_eq!(&state, &before, "E4");
                }
            }
        }
    }

    fn cleanup_rank(state: &SessionCleanupOperation) -> u8 {
        match state.state() {
            SessionCleanupState::NotRequested => 0,
            SessionCleanupState::Fenced { .. } => 1,
            SessionCleanupState::Requested { .. } => 2,
            SessionCleanupState::Completed { .. } => 3,
            SessionCleanupState::RepositoryPublication(publication) => {
                cleanup_rank(&publication.cleanup)
            }
            SessionCleanupState::Preparing(_) => 2,
            SessionCleanupState::Disposing(_) => 2,
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn session_cleanup_completion_requires_every_identity_axis() {
        let artifact_effects_unique = kani::any::<bool>();
        let session_matches = kani::any::<bool>();
        let thread_matches = kani::any::<bool>();
        let effect_matches = kani::any::<bool>();
        let canonical_receipt_matches = kani::any::<bool>();
        let admitted = session_cleanup_completion_admitted(
            artifact_effects_unique,
            session_matches,
            thread_matches,
            effect_matches,
            canonical_receipt_matches,
        );
        assert_eq!(
            admitted,
            artifact_effects_unique
                && session_matches
                && thread_matches
                && effect_matches
                && canonical_receipt_matches
        );
    }

    #[kani::proof]
    fn session_cleanup_phase_advances_only_not_requested_fenced_requested_completed() {
        let current_code = kani::any::<u8>();
        let next_code = kani::any::<u8>();
        kani::assume(current_code < 4);
        kani::assume(next_code < 4);
        let current = match current_code {
            0 => SessionCleanupPhase::NotRequested,
            1 => SessionCleanupPhase::Fenced,
            2 => SessionCleanupPhase::Requested,
            3 => SessionCleanupPhase::Completed,
            _ => unreachable!(),
        };
        let next = match next_code {
            0 => SessionCleanupPhase::NotRequested,
            1 => SessionCleanupPhase::Fenced,
            2 => SessionCleanupPhase::Requested,
            3 => SessionCleanupPhase::Completed,
            _ => unreachable!(),
        };
        assert_eq!(
            session_cleanup_phase_advance_admitted(current, next),
            next_code == current_code + 1
        );
    }
}
