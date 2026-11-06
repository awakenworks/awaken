// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: FileCatalog owns complete output capture. The canonical reducer validates
// @formal reason: Workspace/Session identity, nonwrapping revision, unique File ids and exact replay
// @formal reason: before CAS; transport adapters verify the same receipt. It creates no second artifact
// @formal reason: store.
//! Complete output observations in the existing FileCatalog. These are File
//! references, not another artifact store, execution ledger, or success claim.

use std::collections::BTreeSet;

use crate::{FileCatalogError, FileRecord, harvest_idempotency_key};

/// Original publication coordinates, not execution authority. The driving
/// boundary derives this reference from its already-authorized Run claim.
#[derive(Debug, Clone, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCaptureRunReference {
    pub run_id: String,
    pub owner: String,
    pub epoch: std::num::NonZeroU64,
}

impl ArtifactCaptureRunReference {
    pub fn identity(&self) -> (&str, &str, std::num::NonZeroU64) {
        (&self.run_id, &self.owner, self.epoch)
    }

    fn is_valid(&self) -> bool {
        [&self.run_id, &self.owner]
            .into_iter()
            .all(|value| !value.trim().is_empty() && value.trim() == value.as_str())
    }
}

impl PartialEq for ArtifactCaptureRunReference {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCapture {
    pub workspace_id: String,
    pub session_id: String,
    pub revision: u64,
    pub file_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ArtifactCaptureRunReference>,
}

/// Read the current revision BEFORE enumerating outputs, then publish all File
/// receipts before submitting this CAS. A conflict requires a fresh enumeration;
/// changing only expected_revision would let stale bytes replace a newer capture.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplaceArtifactCapture {
    pub workspace_id: String,
    pub session_id: String,
    pub expected_revision: u64,
    pub file_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ArtifactCaptureRunReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "capture", rename_all = "snake_case")]
pub enum ReplaceArtifactCaptureOutcome {
    Applied(ArtifactCapture),
    Replayed(ArtifactCapture),
    Conflict,
}

impl ReplaceArtifactCapture {
    pub fn new(
        workspace_id: String,
        session_id: String,
        expected_revision: u64,
        file_ids: Vec<String>,
    ) -> Self {
        Self {
            workspace_id,
            session_id,
            expected_revision,
            file_ids,
            origin: None,
        }
    }
    /// Attach the original execution boundary's coordinates. This metadata
    /// binding owns no execution authority; driving edges retain their guards.
    pub fn bind_run_origin(
        &mut self,
        claim: Option<(&str, &str, u64)>,
    ) -> Result<(), crate::ArtifactPublicationError> {
        let original = match claim {
            Some((run_id, owner, epoch)) => Some(ArtifactCaptureRunReference {
                run_id: run_id.into(),
                owner: owner.into(),
                epoch: std::num::NonZeroU64::new(epoch).ok_or_else(|| {
                    crate::ArtifactPublicationError::InvalidRequest {
                        detail: "artifact capture has no original nonzero Run epoch".into(),
                    }
                })?,
            }),
            None => None,
        };
        if self
            .origin
            .as_ref()
            .is_some_and(|supplied| Some(supplied) != original.as_ref())
        {
            return Err(crate::ArtifactPublicationError::Denied {
                detail: "artifact capture origin differs from its original effect fence".into(),
            });
        }
        self.origin = original;
        Ok(())
    }

    /// A peer cannot acknowledge another revision, scope, or selected set.
    /// Conflict carries no replacement receipt and remains a rejected write.
    pub fn verify_outcome(
        &self,
        outcome: &ReplaceArtifactCaptureOutcome,
    ) -> Result<(), FileCatalogError> {
        let capture = match outcome {
            ReplaceArtifactCaptureOutcome::Applied(capture)
            | ReplaceArtifactCaptureOutcome::Replayed(capture) => capture,
            ReplaceArtifactCaptureOutcome::Conflict => return Ok(()),
        };
        let actual = self
            .decide(Some(capture))
            .map_err(|error| FileCatalogError::CorruptData {
                detail: error.to_string(),
            })?;
        if actual != ReplaceArtifactCaptureOutcome::Replayed(capture.clone()) {
            return Err(FileCatalogError::CorruptData {
                detail: "noncanonical output capture receipt".into(),
            });
        }
        Ok(())
    }

    /// One reducer for every catalog adapter. A replay after logical File
    /// deletion still acknowledges the old observation; it never revives bytes.
    pub fn decide(
        &self,
        current: Option<&ArtifactCapture>,
    ) -> Result<ReplaceArtifactCaptureOutcome, FileCatalogError> {
        if self.workspace_id.trim().is_empty()
            || self.session_id.trim().is_empty()
            || self.file_ids.iter().any(|id| id.trim().is_empty())
            || self.expected_revision >= i64::MAX as u64
            || self
                .origin
                .as_ref()
                .is_some_and(|origin| !origin.is_valid())
        {
            return Err(FileCatalogError::InvalidCapture {
                detail: "invalid output capture identity or revision".into(),
            });
        }
        let mut file_ids = self.file_ids.clone();
        file_ids.sort();
        if file_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(FileCatalogError::InvalidCapture {
                detail: "duplicate output File reference".into(),
            });
        }
        if let Some(current) = current
            && (current.workspace_id != self.workspace_id
                || current.session_id != self.session_id
                || current
                    .origin
                    .as_ref()
                    .is_some_and(|origin| !origin.is_valid()))
        {
            return Err(FileCatalogError::CorruptData {
                detail: "output capture belongs to another scope or has corrupt provenance".into(),
            });
        }
        let origin = self.origin.clone().or_else(|| {
            current
                .filter(|capture| capture.file_ids == file_ids)
                .and_then(|capture| capture.origin.clone())
        });
        let next = ArtifactCapture {
            workspace_id: self.workspace_id.clone(),
            session_id: self.session_id.clone(),
            revision: self.expected_revision + 1,
            file_ids,
            origin,
        };
        if let Some(current) = current
            && current == &next
        {
            return Ok(ReplaceArtifactCaptureOutcome::Replayed(current.clone()));
        }
        if current.map_or(0, |capture| capture.revision) != self.expected_revision {
            return Ok(ReplaceArtifactCaptureOutcome::Conflict);
        }
        Ok(ReplaceArtifactCaptureOutcome::Applied(next))
    }
}

impl ArtifactCapture {
    /// Verify a stored or transported observation against its requested scope.
    /// Reuse the replacement reducer for canonical ids and exact revision rules.
    pub fn verify_scope(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<(), FileCatalogError> {
        if self.workspace_id != workspace_id || self.session_id != session_id {
            return Err(FileCatalogError::CorruptData {
                detail: "output capture belongs to another scope".into(),
            });
        }
        let expected_revision =
            self.revision
                .checked_sub(1)
                .ok_or_else(|| FileCatalogError::CorruptData {
                    detail: "uncommitted output capture revision".into(),
                })?;
        let request = ReplaceArtifactCapture {
            origin: self.origin.clone(),
            workspace_id: self.workspace_id.clone(),
            session_id: self.session_id.clone(),
            expected_revision,
            file_ids: self.file_ids.clone(),
        };
        request.verify_outcome(&ReplaceArtifactCaptureOutcome::Replayed(self.clone()))
    }

    /// Called under the SAME transaction/lock as catalog replacement. File
    /// deletion cannot race validation and make a newly selected File invalid.
    /// Existing captures deliberately retain a deleted File's exact identity;
    /// readers must report it unavailable, never substitute an older version.
    pub fn verify_files(&self, records: &[FileRecord]) -> Result<(), FileCatalogError> {
        let ids = records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<BTreeSet<_>>();
        let expected = self
            .file_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if ids != expected || records.len() != self.file_ids.len() {
            return Err(FileCatalogError::InvalidCapture {
                detail: "output capture references missing Files".into(),
            });
        }
        let mut paths = BTreeSet::new();
        for record in records {
            let path = record.logical_path.as_deref().unwrap_or_default();
            if record.workspace_id != self.workspace_id
                || record.scope_id.as_deref() != Some(self.session_id.as_str())
                || record.deleted
                || !record.downloadable
                || record.expires_at.is_some()
                || !crate::execution::valid_artifact_logical_path(path)
                || record.filename != path
                || record.harvest_key.as_deref()
                    != Some(
                        harvest_idempotency_key(&self.session_id, path, &record.blob_id).as_str(),
                    )
                || !paths.insert(path)
            {
                return Err(FileCatalogError::InvalidCapture {
                    detail:
                        "output capture requires distinct live artifact paths in its exact scope"
                            .into(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_cas_retains_the_original_publishing_run_reference() {
        // C1 original boundary supplies a publishing Run reference; C2 valid
        // scope, complete File ids and expected revision. R1 C1+C2 -> same
        // catalog CAS retains that reference, including serialization/cold
        // read. This pure reducer does not authorize or mint a Run claim;
        // live-claim and forged-reference cases belong to the driving edges.
        let origin = serde_json::json!({"run_id":"run","owner":"worker","epoch":7});
        let request: ReplaceArtifactCapture = serde_json::from_value(serde_json::json!({
            "workspace_id":"w","session_id":"s","expected_revision":0,
            "file_ids":[],"origin":origin,
        }))
        .unwrap();
        let ReplaceArtifactCaptureOutcome::Applied(capture) = request.decide(None).unwrap() else {
            panic!("R1 new capture");
        };
        let stored = serde_json::to_value(capture).unwrap();
        assert_eq!(
            stored["origin"], origin,
            "R1 original reference cannot be erased"
        );
        let loaded: ArtifactCapture = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            stored,
            "R1 cold read"
        );
    }

    #[test]
    fn capture_origin_follows_selection_cas_and_never_backfills_unknown_history() {
        // C1 stored origin present/legacy unknown; C2 new origin supplied;
        // C3 selected File set unchanged/changed; C4 expected revision current;
        // C5 canonical origin. E1 retain an original reference; E2 remain or
        // become unknown; E3 conflict, no write; E4 reject invalid input or
        // corrupt observation. Rules: P1 C2+C4+C5 -> E1; P2 !C2+C3+C1 -> E1;
        // P3 !C2+(!C3|!C1) -> E2; P4 non-replay wrong revision -> E3;
        // P5 !C5 -> E4. Exact prior request replay retains the existing CAS law. Terminal
        // preservation refers only to the exact existing complete File set.
        let origin: ArtifactCaptureRunReference = serde_json::from_value(serde_json::json!({
            "run_id":"run","owner":"worker","epoch":7,
        }))
        .unwrap();
        let old = ArtifactCapture {
            workspace_id: "w".into(),
            session_id: "s".into(),
            revision: 1,
            file_ids: vec!["a".into()],
            origin: Some(origin.clone()),
        };
        for has_old in [false, true] {
            for new_origin in [false, true] {
                for same_files in [false, true] {
                    for current in [false, true] {
                        let mut old = old.clone();
                        if !has_old {
                            old.origin = None;
                        }
                        let mut request = ReplaceArtifactCapture::new(
                            "w".into(),
                            "s".into(),
                            if current { 1 } else { 3 },
                            vec![if same_files { "a" } else { "b" }.into()],
                        );
                        if new_origin {
                            let mut new = origin.clone();
                            new.run_id = "new-run".into();
                            request.origin = Some(new);
                        }
                        let result = request.decide(Some(&old)).unwrap();
                        if !current {
                            assert_eq!(result, ReplaceArtifactCaptureOutcome::Conflict, "P4");
                            continue;
                        }
                        let ReplaceArtifactCaptureOutcome::Applied(capture) = result else {
                            panic!("P1-P3 new revision");
                        };
                        let expected = request
                            .origin
                            .clone()
                            .or_else(|| (same_files && has_old).then(|| origin.clone()));
                        assert_eq!(capture.origin, expected, "P1-P3");
                        request
                            .verify_outcome(&ReplaceArtifactCaptureOutcome::Applied(
                                capture.clone(),
                            ))
                            .unwrap();
                        assert_eq!(
                            request.decide(Some(&capture)).unwrap(),
                            ReplaceArtifactCaptureOutcome::Replayed(capture),
                            "P1-P3 exact replay"
                        );
                    }
                }
            }
        }
        for field in ["run_id", "owner", "epoch"] {
            let mut malformed = serde_json::to_value(&origin).unwrap();
            malformed[field] = if field == "epoch" {
                serde_json::json!(0)
            } else {
                serde_json::json!(" ")
            };
            match serde_json::from_value::<ArtifactCaptureRunReference>(malformed) {
                Err(_) => assert_eq!(field, "epoch", "P5 nonzero wire type"),
                Ok(invalid) => {
                    let mut request =
                        ReplaceArtifactCapture::new("w".into(), "s".into(), 1, vec!["a".into()]);
                    request.origin = Some(invalid.clone());
                    assert!(
                        matches!(
                            request.decide(Some(&old)),
                            Err(FileCatalogError::InvalidCapture { .. })
                        ),
                        "P5 input"
                    );
                    let mut corrupt = old.clone();
                    corrupt.origin = Some(invalid);
                    assert!(
                        matches!(
                            corrupt.verify_scope("w", "s"),
                            Err(FileCatalogError::CorruptData { .. })
                        ),
                        "P5 observation"
                    );
                }
            }
        }
    }

    #[test]
    fn capture_origin_binding_cannot_substitute_original_coordinates() {
        // B1 supplied original coordinates + absent/same metadata -> exact
        // binding; B2 any foreign run/owner/epoch -> refuse; B3 no Run cannot
        // borrow an existing reference; B4 zero epoch -> invalid. Live claim
        // authorization remains in Coordinator/Worker transport tests.
        let original = Some(("run", "worker", 7));
        let mut request = ReplaceArtifactCapture::new("w".into(), "s".into(), 0, vec![]);
        request.bind_run_origin(original).unwrap();
        let expected = request.origin.clone().unwrap();
        assert_eq!(
            (&*expected.run_id, &*expected.owner, expected.epoch.get()),
            ("run", "worker", 7),
            "B1"
        );
        request.bind_run_origin(original).unwrap();
        for replacement in [
            Some(("foreign", "worker", 7)),
            Some(("run", "foreign", 7)),
            Some(("run", "worker", 8)),
            None,
        ] {
            let mut changed = request.clone();
            assert!(changed.bind_run_origin(replacement).is_err(), "B2/B3");
            assert_eq!(changed, request, "B2/B3 refusal leaves metadata unchanged");
        }
        assert!(
            request.bind_run_origin(Some(("run", "worker", 0))).is_err(),
            "B4"
        );
        request.origin = None;
        request.bind_run_origin(None).unwrap();
        assert!(request.origin.is_none(), "B3 unknown remains unknown");
    }

    #[test]
    fn request_and_observation_failures_have_different_meanings() {
        // C1 invalid caller revision => Invalid (no execution); C2 stored
        // invalid revision or wrong scope => CorruptData; C3 wrong receipt =>
        // CorruptData. The same words cannot erase the observation boundary.
        let request = ReplaceArtifactCapture {
            origin: None,
            workspace_id: "w".into(),
            session_id: "s".into(),
            expected_revision: u64::MAX,
            file_ids: vec![],
        };
        assert!(matches!(
            request.decide(None),
            Err(FileCatalogError::InvalidCapture { .. })
        ));
        let mut observed = ArtifactCapture {
            origin: None,
            workspace_id: "w".into(),
            session_id: "s".into(),
            revision: 0,
            file_ids: vec![],
        };
        assert!(matches!(
            observed.verify_scope("w", "s"),
            Err(FileCatalogError::CorruptData { .. })
        ));
        observed.revision = 1;
        assert!(matches!(
            observed.verify_scope("other", "s"),
            Err(FileCatalogError::CorruptData { .. })
        ));
        let valid = ReplaceArtifactCapture {
            expected_revision: 0,
            ..request
        };
        observed.revision = 2;
        assert!(matches!(
            valid.verify_outcome(&ReplaceArtifactCaptureOutcome::Applied(observed)),
            Err(FileCatalogError::CorruptData { .. })
        ));
    }
}
