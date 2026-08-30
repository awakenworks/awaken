//! Exact per-thread completion evidence for the terminal cleanup operation.
//!
//! This module owns receipt canonicalization only. Cleanup phase authority and
//! aggregate transitions remain in the parent module.

use super::{SessionCleanupCommand, SessionCleanupError, session_cleanup_completion_admitted};
use awaken_resource_contract::ArtifactPublicationReceipt;
use serde::{Deserialize, Deserializer, Serialize};

pub(super) fn canonical_thread_artifact_receipt_fingerprint(
    domain: &str,
    command: &SessionCleanupCommand,
    artifact_receipts: &mut [ArtifactPublicationReceipt],
) -> (bool, String) {
    let receipts_are_unique = ArtifactPublicationReceipt::canonicalize(artifact_receipts);
    let artifact_evidence = artifact_receipts
        .iter()
        .map(|receipt| (receipt.effect_id.as_str(), receipt.content_id.as_str()))
        .collect::<Vec<_>>();
    let fingerprint = command.restore_target.as_ref().map_or_else(
        || {
            crate::stable_fingerprint(&(
                domain,
                command.session_id.as_str(),
                command.thread_id.as_str(),
                command.effect_id.as_str(),
                artifact_evidence.as_slice(),
            ))
        },
        |request| {
            crate::stable_fingerprint(&(
                "session-terminal-cleanup-thread-restore-v1",
                domain,
                command.session_id.as_str(),
                command.thread_id.as_str(),
                command.effect_id.as_str(),
                request,
                artifact_evidence.as_slice(),
            ))
        },
    );
    (receipts_are_unique, fingerprint)
}

/// Untrusted Runtime report that one per-thread cleanup command completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct SessionCleanupCompletion {
    pub(super) session_id: String,
    pub(super) thread_id: String,
    pub(super) effect_id: String,
    pub(super) artifact_receipts: Vec<ArtifactPublicationReceipt>,
    pub(super) receipt_fingerprint: String,
}

/// Decode-only shape for the aggregate receipt removed with the redundant
/// three-file bundle path. It is deliberately private and never serializes:
/// persisted rows and in-flight Workers from the previous release converge on
/// the same current completion instead of creating two compatibility paths.
#[derive(Deserialize)]
struct RemovedArtifactBundleCompletionReceipt {
    #[serde(rename = "purpose")]
    _purpose: RemovedArtifactBundlePurpose,
    #[serde(rename = "patch_sha256")]
    _patch_sha256: String,
    #[serde(rename = "patch_artifact_id")]
    _patch_artifact_id: String,
    #[serde(rename = "manifest_artifact_id")]
    _manifest_artifact_id: String,
    #[serde(rename = "checksum_artifact_id")]
    _checksum_artifact_id: String,
    receipt_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RemovedArtifactBundlePurpose {
    SkillExport,
    PatchBundle,
}

#[derive(Deserialize)]
struct SessionCleanupCompletionWire {
    session_id: String,
    thread_id: String,
    effect_id: String,
    artifact_receipts: Vec<ArtifactPublicationReceipt>,
    #[serde(default)]
    artifact_bundle_receipts: Vec<RemovedArtifactBundleCompletionReceipt>,
    receipt_fingerprint: String,
}

impl<'de> Deserialize<'de> for SessionCleanupCompletion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SessionCleanupCompletionWire::deserialize(deserializer)?;
        if wire.artifact_bundle_receipts.is_empty() {
            return Ok(Self {
                session_id: wire.session_id,
                thread_id: wire.thread_id,
                effect_id: wire.effect_id,
                artifact_receipts: wire.artifact_receipts,
                receipt_fingerprint: wire.receipt_fingerprint,
            });
        }

        // The old verifier first sorted both evidence sets, rejected duplicate
        // identities, and then compared the entire canonical value. Validate
        // those same conditions before discarding the removed redundant field.
        let artifacts_are_canonical = wire
            .artifact_receipts
            .windows(2)
            .all(|pair| pair[0].effect_id < pair[1].effect_id);
        let bundles_are_canonical = wire
            .artifact_bundle_receipts
            .windows(2)
            .all(|pair| pair[0].receipt_fingerprint < pair[1].receipt_fingerprint);
        let artifact_evidence = wire
            .artifact_receipts
            .iter()
            .map(|receipt| (receipt.effect_id.as_str(), receipt.content_id.as_str()))
            .collect::<Vec<_>>();
        let bundle_evidence = wire
            .artifact_bundle_receipts
            .iter()
            .map(|receipt| receipt.receipt_fingerprint.as_str())
            .collect::<Vec<_>>();
        let expected_v2 = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v2",
            wire.session_id.as_str(),
            wire.thread_id.as_str(),
            wire.effect_id.as_str(),
            artifact_evidence,
            bundle_evidence,
        ));
        if !artifacts_are_canonical
            || !bundles_are_canonical
            || wire.receipt_fingerprint != expected_v2
        {
            return Err(serde::de::Error::custom(
                "legacy Session cleanup completion is not canonical",
            ));
        }

        let command = SessionCleanupCommand {
            session_id: wire.session_id,
            thread_id: wire.thread_id,
            effect_id: wire.effect_id,
            restore_target: None,
        };
        Ok(Self::new(&command, wire.artifact_receipts))
    }
}

impl SessionCleanupCompletion {
    #[must_use]
    pub(super) fn new(
        command: &SessionCleanupCommand,
        mut artifact_receipts: Vec<ArtifactPublicationReceipt>,
    ) -> Self {
        let (_receipts_are_unique, receipt_fingerprint) =
            canonical_thread_artifact_receipt_fingerprint(
                "session-terminal-cleanup-thread-receipt-v1",
                command,
                &mut artifact_receipts,
            );
        Self {
            session_id: command.session_id.clone(),
            thread_id: command.thread_id.clone(),
            effect_id: command.effect_id.clone(),
            artifact_receipts,
            receipt_fingerprint,
        }
    }

    pub(super) fn verify(
        &self,
        command: &SessionCleanupCommand,
    ) -> Result<VerifiedSessionCleanupReceipt, SessionCleanupError> {
        let mut canonical_receipts = self.artifact_receipts.clone();
        let (receipts_are_unique, canonical_fingerprint) =
            canonical_thread_artifact_receipt_fingerprint(
                "session-terminal-cleanup-thread-receipt-v1",
                command,
                &mut canonical_receipts,
            );
        if !session_cleanup_completion_admitted(
            receipts_are_unique,
            self.session_id == command.session_id,
            self.thread_id == command.thread_id,
            self.effect_id == command.effect_id,
            self.artifact_receipts == canonical_receipts
                && self.receipt_fingerprint == canonical_fingerprint,
        ) {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
        Ok(VerifiedSessionCleanupReceipt {
            command: command.clone(),
            completion: self.clone(),
        })
    }
}

/// Exact, process-local completion evidence admitted against one cleanup
/// command. It is intentionally not serializable and has no public constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VerifiedSessionCleanupReceipt {
    command: SessionCleanupCommand,
    completion: SessionCleanupCompletion,
}

impl VerifiedSessionCleanupReceipt {
    #[must_use]
    pub(super) fn command(&self) -> &SessionCleanupCommand {
        &self.command
    }

    pub(super) fn thread_id(&self) -> &str {
        &self.completion.thread_id
    }

    pub(super) fn effect_id(&self) -> &str {
        &self.completion.effect_id
    }

    pub(super) fn receipt_fingerprint(&self) -> &str {
        &self.completion.receipt_fingerprint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::FileRecord;
    use serde_json::json;

    fn artifact(effect_id: &str, content_id: &str) -> ArtifactPublicationReceipt {
        ArtifactPublicationReceipt {
            effect_id: effect_id.into(),
            content_id: content_id.into(),
            record: FileRecord {
                id: format!("file-{effect_id}"),
                workspace_id: "workspace".into(),
                blob_id: content_id.into(),
                filename: format!("{effect_id}.txt"),
                mime_type: "text/plain".into(),
                size_bytes: 1,
                created_at: "2026-08-29T00:00:00Z".into(),
                expires_at: None,
                downloadable: true,
                scope_id: Some("session".into()),
                logical_path: Some(format!("outputs/{effect_id}.txt")),
                harvest_key: Some(effect_id.into()),
                artifact_idempotency_scope: None,
                deleted: false,
            },
        }
    }

    #[test]
    fn restoring_target_is_an_exact_completion_axis() {
        // Cause/effect table: T1 root command has no restore target => legacy
        // completion domain; T2 it carries the exact durable Restoring tuple =>
        // target-bound receipt; T3 either receipt is checked against the other
        // command => reject. This prevents a Phase-A Worker from acknowledging
        // cleanup without disposing the already-created exact target.
        let base = SessionCleanupCommand::new("session", "session", "root");
        let exact = base
            .clone()
            .with_restore_target(crate::SandboxRestoreRequest {
                workspace_id: "workspace".into(),
                session_id: "session".into(),
                effect_id: awaken_agent_contract::collision_resistant_fingerprint(
                    "awaken-terminal-restore-test-v1",
                    &[b"effect"],
                ),
                generation_id: "generation".into(),
                checkpoint: crate::SandboxCheckpointRef {
                    id: "checkpoint".into(),
                    format: "test".into(),
                    digest: "digest".into(),
                    size_bytes: 1,
                    created_at_unix_ms: 1,
                    expires_at_unix_ms: 2,
                    environment_fingerprint: "environment".into(),
                    base_image_fingerprint: "image".into(),
                    excluded_mounts: Vec::new(),
                    suspend_effect_id: "suspend".into(),
                },
            })
            .unwrap();
        let legacy = SessionCleanupCompletion::new(&base, Vec::new());
        let bound = SessionCleanupCompletion::new(&exact, Vec::new());
        assert_ne!(
            legacy.receipt_fingerprint, bound.receipt_fingerprint,
            "T1/T2"
        );
        assert!(legacy.verify(&exact).is_err(), "T3");
        assert!(bound.verify(&base).is_err(), "T3");
        bound.verify(&exact).expect("T2");
    }

    fn removed_bundle(fingerprint: &str) -> serde_json::Value {
        json!({
            "purpose": "patch_bundle",
            "patch_sha256": "sha256:legacy",
            "patch_artifact_id": "file-patch",
            "manifest_artifact_id": "file-manifest",
            "checksum_artifact_id": "file-checksum",
            "receipt_fingerprint": fingerprint,
        })
    }

    fn legacy_v2(
        artifacts: Vec<ArtifactPublicationReceipt>,
        bundle_fingerprints: &[&str],
    ) -> serde_json::Value {
        let artifact_evidence = artifacts
            .iter()
            .map(|receipt| (receipt.effect_id.as_str(), receipt.content_id.as_str()))
            .collect::<Vec<_>>();
        let fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v2",
            "session",
            "thread",
            "effect",
            artifact_evidence,
            bundle_fingerprints.to_vec(),
        ));
        json!({
            "session_id": "session",
            "thread_id": "thread",
            "effect_id": "effect",
            "artifact_receipts": artifacts,
            "artifact_bundle_receipts": bundle_fingerprints
                .iter()
                .map(|fingerprint| removed_bundle(fingerprint))
                .collect::<Vec<_>>(),
            "receipt_fingerprint": fingerprint,
        })
    }

    #[test]
    fn removed_bundle_wire_has_one_fail_closed_compatibility_decoder() {
        // Causes: C1 input is current v1 or exact historical v2; C2 v2
        // artifact effects and removed receipt fingerprints are strictly
        // canonical; C3 the historical fingerprint binds every retained
        // identity. Effects: E1 current input is unchanged; E2 exact v2 becomes
        // the same v1 domain value and can only serialize as v1; E3 malformed,
        // reordered, duplicate, or forged v2 is rejected. Decision rules:
        // D1=C1(current)=>E1; D2=C1(v2)+C2+C3=>E2;
        // D3=C1(v2)+(!C2||!C3)=>E3. Store and Worker adapters both deserialize
        // this type, so these rules are the sole rolling-upgrade authority.
        let command = SessionCleanupCommand {
            session_id: "session".into(),
            thread_id: "thread".into(),
            effect_id: "effect".into(),
            restore_target: None,
        };
        let current = SessionCleanupCompletion::new(&command, Vec::new());
        assert_eq!(
            serde_json::from_value::<SessionCleanupCompletion>(
                serde_json::to_value(&current).unwrap()
            )
            .unwrap(),
            current,
            "D1/E1",
        );
        let mut explicit_empty = serde_json::to_value(&current).unwrap();
        explicit_empty["artifact_bundle_receipts"] = json!([]);
        assert_eq!(
            serde_json::from_value::<SessionCleanupCompletion>(explicit_empty).unwrap(),
            current,
            "D1/E1 explicit empty historical field",
        );
        let mut forged_current = serde_json::to_value(&current).unwrap();
        forged_current["receipt_fingerprint"] = json!("forged-current");
        let forged_current = serde_json::from_value::<SessionCleanupCompletion>(forged_current)
            .expect("D1 current decode remains structural");
        assert!(
            forged_current.verify(&command).is_err(),
            "D1 command verification remains the current semantic gate",
        );

        let artifacts = vec![artifact("a", "sha256:a"), artifact("b", "sha256:b")];
        let mut historical = legacy_v2(artifacts.clone(), &["bundle-a", "bundle-b"]);
        historical["ignored_outer"] = json!(true);
        historical["artifact_bundle_receipts"][0]["ignored_nested"] = json!(true);
        let normalized = serde_json::from_value::<SessionCleanupCompletion>(historical)
            .expect("D2/E2 exact historical value");
        assert_eq!(
            normalized,
            SessionCleanupCompletion::new(&command, artifacts),
            "D2/E2",
        );
        let foreign_command = SessionCleanupCommand {
            session_id: "session".into(),
            thread_id: "foreign-thread".into(),
            effect_id: "foreign-effect".into(),
            restore_target: None,
        };
        assert!(
            normalized.verify(&foreign_command).is_err(),
            "D2 decode preserves domain-owned command binding",
        );
        assert!(
            serde_json::to_value(&normalized)
                .unwrap()
                .get("artifact_bundle_receipts")
                .is_none(),
            "D2/E2 current-only serialization",
        );

        let reordered_artifacts = legacy_v2(
            vec![artifact("b", "sha256:b"), artifact("a", "sha256:a")],
            &["bundle-a"],
        );
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(reordered_artifacts).is_err(),
            "D3/E3 artifact order",
        );
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(legacy_v2(
                vec![artifact("a", "sha256:a"), artifact("a", "sha256:b")],
                &["bundle-a"],
            ))
            .is_err(),
            "D3/E3 duplicate artifact",
        );
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(legacy_v2(
                Vec::new(),
                &["bundle-b", "bundle-a"],
            ))
            .is_err(),
            "D3/E3 bundle order",
        );
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(legacy_v2(
                Vec::new(),
                &["duplicate", "duplicate"],
            ))
            .is_err(),
            "D3/E3 duplicate bundle",
        );
        let mut forged = legacy_v2(Vec::new(), &["bundle-a"]);
        forged["receipt_fingerprint"] = json!("forged");
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(forged).is_err(),
            "D3/E3 fingerprint",
        );
        let mut malformed = legacy_v2(Vec::new(), &["bundle-a"]);
        malformed["artifact_bundle_receipts"][0]
            .as_object_mut()
            .unwrap()
            .remove("manifest_artifact_id");
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(malformed).is_err(),
            "D3/E3 required historical shape",
        );
        let mut unknown_purpose = legacy_v2(Vec::new(), &["bundle-a"]);
        unknown_purpose["artifact_bundle_receipts"][0]["purpose"] = json!("unknown");
        assert!(
            serde_json::from_value::<SessionCleanupCompletion>(unknown_purpose).is_err(),
            "D3/E3 closed historical purpose",
        );
    }

    #[test]
    fn multi_artifact_live_and_recovery_orders_share_one_canonical_receipt_policy() {
        // Cause/effect decision table:
        // C1 live harvest and durable recovery return the same distinct effects
        // in different orders => E1 one canonical order and identical completion
        // fingerprint; C2 either path carries duplicate effect identity => E2
        // completion verification rejects. ArtifactPublicationReceipt owns both
        // sorting and duplicate classification; Session cleanup only consumes it.
        let command = SessionCleanupCommand {
            session_id: "session".into(),
            thread_id: "thread".into(),
            effect_id: "cleanup".into(),
            restore_target: None,
        };
        let live = SessionCleanupCompletion::new(
            &command,
            vec![artifact("b", "sha256:b"), artifact("a", "sha256:a")],
        );
        let recovered = SessionCleanupCompletion::new(
            &command,
            vec![artifact("a", "sha256:a"), artifact("b", "sha256:b")],
        );
        assert_eq!(live, recovered, "E1 order-independent completion");
        live.verify(&command).expect("E1 canonical completion");

        let duplicate = SessionCleanupCompletion::new(
            &command,
            vec![artifact("a", "sha256:a"), artifact("a", "sha256:other")],
        );
        assert!(duplicate.verify(&command).is_err(), "E2 duplicate rejected");
    }
}
