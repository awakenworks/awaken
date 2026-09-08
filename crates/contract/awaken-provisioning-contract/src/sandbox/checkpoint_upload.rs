//! Exact upload evidence persisted by a Sandbox's existing realization owner.
//! This value owns comparisons only: no storage, locking, liveness or deletion.

use serde::{Deserialize, Serialize};

use super::{SandboxCheckpointRef, SandboxError};

/// The existing provider participant for one checkpoint upload. A provider
/// persists pending evidence before object I/O, then persists the complete
/// receipt before acknowledging Session. Current authority and physical
/// custody must be validated by that provider at every persistence/I/O edge.
///
/// The wire shape retains historical filesystem-marker compatibility. Missing
/// operation identity is readable history, never authority for a new upload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxCheckpointUpload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    request_fingerprint: String,
    snapshot_digest: String,
    snapshot_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference: Option<SandboxCheckpointRef>,
}

impl SandboxCheckpointUpload {
    /// Prepare the same evidence previously owned by the filesystem marker.
    /// The caller derives these facts from an admitted request and exact bytes;
    /// construction itself grants neither physical access nor an upload.
    #[must_use]
    pub fn pending(
        operation_id: String,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Self {
        Self {
            operation_id: Some(operation_id),
            request_fingerprint: request_fingerprint.to_owned(),
            snapshot_digest: snapshot_digest.to_owned(),
            snapshot_size_bytes,
            reference: None,
        }
    }

    #[must_use]
    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }

    #[must_use]
    pub fn request_fingerprint(&self) -> &str {
        &self.request_fingerprint
    }

    #[must_use]
    pub fn reference(&self) -> Option<&SandboxCheckpointRef> {
        self.reference.as_ref()
    }

    /// Compare pending input independently of whether its receipt was recorded.
    #[must_use]
    pub fn matches_pending(&self, pending: &Self) -> bool {
        self.operation_id == pending.operation_id
            && self.request_fingerprint == pending.request_fingerprint
            && self.snapshot_digest == pending.snapshot_digest
            && self.snapshot_size_bytes == pending.snapshot_size_bytes
    }

    pub fn completed_for(
        &self,
        request_fingerprint: &str,
        operation_id: &str,
    ) -> Result<Option<SandboxCheckpointRef>, SandboxError> {
        if self.request_fingerprint != request_fingerprint
            || self.operation_id() != Some(operation_id)
        {
            return Err(SandboxError::new(
                "checkpoint participant belongs to another request or operation",
            ));
        }
        Ok(self.reference.clone())
    }

    /// Admit a receipt without overwriting conflicting input or a prior receipt.
    /// `true` requires persistence by the caller before acknowledgement; `false`
    /// is exact replay and does not require a second write. Every error leaves
    /// this value unchanged. Neither result proves current execution authority.
    pub fn complete(
        &mut self,
        operation_id: &str,
        request_fingerprint: &str,
        reference: &SandboxCheckpointRef,
    ) -> Result<bool, SandboxError> {
        if self.operation_id() != Some(operation_id)
            || self.request_fingerprint != request_fingerprint
            || self.snapshot_digest != reference.digest
            || self.snapshot_size_bytes != reference.size_bytes
            || reference.suspend_effect_id != operation_id
        {
            return Err(SandboxError::new(
                "checkpoint receipt does not match pending snapshot evidence",
            ));
        }
        if let Some(recorded) = &self.reference {
            return if recorded == reference {
                Ok(false)
            } else {
                Err(SandboxError::new(
                    "checkpoint completion conflicts with its durable receipt",
                ))
            };
        }
        self.reference = Some(reference.clone());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> SandboxCheckpointUpload {
        SandboxCheckpointUpload::pending("suspend".into(), "request", "sha256:bytes", 17)
    }

    fn receipt() -> SandboxCheckpointRef {
        SandboxCheckpointRef {
            id: "object".into(),
            format: "fixture".into(),
            digest: "sha256:bytes".into(),
            size_bytes: 17,
            created_at_unix_ms: 10,
            expires_at_unix_ms: 20,
            environment_fingerprint: "environment".into(),
            base_image_fingerprint: "image".into(),
            excluded_mounts: vec!["/secret".into()],
            suspend_effect_id: "suspend".into(),
        }
    }

    #[test]
    fn completion_admission_is_exact_and_never_mutates_on_conflict() {
        // Cause/effect graph: C1 operation; C2 request fingerprint; C3 digest;
        // C4 size; C5 receipt suspend effect; C6 a prior receipt exists.
        // Each of C1..C5 must match. R1 all match + !C6 => record, needs write;
        // R2 all match + C6 + exact receipt => replay, no write; R3 any mismatch
        // => error, unchanged value. Enumerate all 32 mismatch combinations
        // for pending/completed state (64 rows), not just one-at-a-time drift.
        for recorded in [false, true] {
            for mismatch in 0_u8..32 {
                let mut value = pending();
                if recorded {
                    assert!(value.complete("suspend", "request", &receipt()).unwrap());
                }
                let before = value.clone();
                let operation = if mismatch & 1 == 0 {
                    "suspend"
                } else {
                    "other"
                };
                let fingerprint = if mismatch & 2 == 0 {
                    "request"
                } else {
                    "other"
                };
                let mut incoming = receipt();
                if mismatch & 4 != 0 {
                    incoming.digest = "sha256:other".into();
                }
                if mismatch & 8 != 0 {
                    incoming.size_bytes += 1;
                }
                if mismatch & 16 != 0 {
                    incoming.suspend_effect_id = "other".into();
                }
                let result = value.complete(operation, fingerprint, &incoming);
                if mismatch == 0 {
                    assert_eq!(result.unwrap(), !recorded, "R1/R2");
                    assert_eq!(value.reference(), Some(&receipt()));
                } else {
                    assert!(result.is_err(), "R3/{recorded}/{mismatch}");
                    assert_eq!(value, before, "R3/{recorded}/{mismatch}");
                }
            }
        }

        // R4 C1..C5 match but any other completed receipt coordinate drifts =>
        // error and no overwrite. These coordinates are not pending-input
        // fields, so this row exercises full receipt equality independently.
        for field in 0..7 {
            let mut value = pending();
            value.complete("suspend", "request", &receipt()).unwrap();
            let before = value.clone();
            let mut incoming = receipt();
            match field {
                0 => incoming.id = "other".into(),
                1 => incoming.format = "other".into(),
                2 => incoming.created_at_unix_ms += 1,
                3 => incoming.expires_at_unix_ms += 1,
                4 => incoming.environment_fingerprint = "other".into(),
                5 => incoming.base_image_fingerprint = "other".into(),
                6 => incoming.excluded_mounts.clear(),
                _ => unreachable!(),
            }
            assert!(
                value.complete("suspend", "request", &incoming).is_err(),
                "R4/{field}"
            );
            assert_eq!(value, before, "R4/{field}");
        }
    }

    #[test]
    fn pending_replay_and_receipt_lookup_have_independent_exact_inputs() {
        // P1/P2: pending replay compares operation, request, digest and size;
        // receipt presence cannot change the answer. Enumerate their 16
        // equality combinations for both receipt states. P3/P4: receipt lookup
        // compares operation/request only, then returns absent/exact receipt.
        for recorded in [false, true] {
            let mut value = pending();
            if recorded {
                value.complete("suspend", "request", &receipt()).unwrap();
            }
            for mismatch in 0_u8..16 {
                let input = SandboxCheckpointUpload::pending(
                    if mismatch & 1 == 0 {
                        "suspend"
                    } else {
                        "other"
                    }
                    .into(),
                    if mismatch & 2 == 0 {
                        "request"
                    } else {
                        "other"
                    },
                    if mismatch & 4 == 0 {
                        "sha256:bytes"
                    } else {
                        "sha256:other"
                    },
                    if mismatch & 8 == 0 { 17 } else { 18 },
                );
                assert_eq!(
                    value.matches_pending(&input),
                    mismatch == 0,
                    "P1/P2/{mismatch}"
                );
            }
            for operation in ["suspend", "other"] {
                for fingerprint in ["request", "other"] {
                    let result = value.completed_for(fingerprint, operation);
                    if operation == "suspend" && fingerprint == "request" {
                        assert_eq!(result.unwrap(), recorded.then(receipt), "P3");
                    } else {
                        assert!(result.is_err(), "P4");
                    }
                }
            }
        }
    }

    #[test]
    fn filesystem_marker_wire_is_unchanged_and_legacy_identity_is_not_promoted() {
        // W1 pending/completed schema-2 participant bytes survive the ownership
        // move unchanged. W2 a historical missing operation still decodes and
        // re-encodes without new fields, but cannot complete/lookup a current
        // operation. W3 unknown fields and missing required evidence fail closed.
        let pending_wire = r#"{"operation_id":"suspend","request_fingerprint":"request","snapshot_digest":"sha256:bytes","snapshot_size_bytes":17}"#;
        assert_eq!(
            serde_json::to_string(&pending()).unwrap(),
            pending_wire,
            "W1"
        );
        let mut completed = pending();
        completed
            .complete("suspend", "request", &receipt())
            .unwrap();
        let completed_wire = format!(
            "{},\"reference\":{}}}",
            pending_wire.trim_end_matches('}'),
            serde_json::to_string(&receipt()).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&completed).unwrap(),
            completed_wire,
            "W1"
        );
        assert_eq!(
            serde_json::from_str::<SandboxCheckpointUpload>(&completed_wire).unwrap(),
            completed
        );

        let legacy_wire = r#"{"request_fingerprint":"request","snapshot_digest":"sha256:bytes","snapshot_size_bytes":17}"#;
        let mut legacy: SandboxCheckpointUpload = serde_json::from_str(legacy_wire).unwrap();
        assert_eq!(legacy.operation_id(), None, "W2");
        assert_eq!(legacy.request_fingerprint(), "request", "W2");
        assert_eq!(serde_json::to_string(&legacy).unwrap(), legacy_wire, "W2");
        assert!(!legacy.matches_pending(&pending()), "W2");
        assert!(legacy.completed_for("request", "suspend").is_err(), "W2");
        assert!(
            legacy.complete("suspend", "request", &receipt()).is_err(),
            "W2"
        );
        assert_eq!(serde_json::to_string(&legacy).unwrap(), legacy_wire, "W2");

        let wire = serde_json::to_value(pending()).unwrap();
        for field in [
            "request_fingerprint",
            "snapshot_digest",
            "snapshot_size_bytes",
        ] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<SandboxCheckpointUpload>(missing).is_err(),
                "W3/{field}"
            );
        }
        let mut extra = wire;
        extra["parallel_receipt"] = true.into();
        assert!(
            serde_json::from_value::<SandboxCheckpointUpload>(extra).is_err(),
            "W3"
        );
    }
}
