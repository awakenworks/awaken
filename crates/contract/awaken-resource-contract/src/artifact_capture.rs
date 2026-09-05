//! Complete output observations in the existing FileCatalog. These are File
//! references, not another artifact store, execution ledger, or success claim.

use std::collections::BTreeSet;

use crate::{FileCatalogError, FileRecord, harvest_idempotency_key};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCapture {
    pub workspace_id: String,
    pub session_id: String,
    pub revision: u64,
    pub file_ids: Vec<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "capture", rename_all = "snake_case")]
pub enum ReplaceArtifactCaptureOutcome {
    Applied(ArtifactCapture),
    Replayed(ArtifactCapture),
    Conflict,
}

impl ReplaceArtifactCapture {
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
            .map_err(|error| FileCatalogError::Storage(error.to_string()))?;
        if actual != ReplaceArtifactCaptureOutcome::Replayed(capture.clone()) {
            return Err(FileCatalogError::Storage(
                "noncanonical output capture receipt".into(),
            ));
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
        {
            return Err(FileCatalogError::Invalid(
                "invalid output capture identity or revision".into(),
            ));
        }
        let mut file_ids = self.file_ids.clone();
        file_ids.sort();
        if file_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(FileCatalogError::Invalid(
                "duplicate output File reference".into(),
            ));
        }
        let next = ArtifactCapture {
            workspace_id: self.workspace_id.clone(),
            session_id: self.session_id.clone(),
            revision: self.expected_revision + 1,
            file_ids,
        };
        if let Some(current) = current {
            if current.workspace_id != self.workspace_id || current.session_id != self.session_id {
                return Err(FileCatalogError::Storage(
                    "output capture belongs to another scope".into(),
                ));
            }
            if current == &next {
                return Ok(ReplaceArtifactCaptureOutcome::Replayed(current.clone()));
            }
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
            return Err(FileCatalogError::Storage(
                "output capture belongs to another scope".into(),
            ));
        }
        let expected_revision = self.revision.checked_sub(1).ok_or_else(|| {
            FileCatalogError::Storage("uncommitted output capture revision".into())
        })?;
        let request = ReplaceArtifactCapture {
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
            return Err(FileCatalogError::Invalid(
                "output capture references missing Files".into(),
            ));
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
                return Err(FileCatalogError::Invalid(
                    "output capture requires distinct live artifact paths in its exact scope"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}
