// @formal entrypoint resource_catalog.artifact_publication_receipt_requires_exact_publication_identity
//! Artifact harvest of the realization executor: the Session outputs a Run or
//! terminal cleanup publishes through the selected Resource authority, and the
//! receipt recovery a terminal retry reads back.

use crate::realization_error::RealizationError;
use awaken_provisioning_contract as pc;
use awaken_resource_contract::ArtifactPublicationError;
use awaken_session_realization_contract::ResourceOperationFence;
use std::sync::Arc;

/// A denied installed-Session read leaves the Artifact authority unknown; the
/// publication retries once a healthy owner can read it.
fn artifact_registry_denied(error: RealizationError) -> ArtifactPublicationError {
    ArtifactPublicationError::AuthorityUnavailable {
        detail: error.to_string(),
    }
}

fn mime_type_for_path(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "md" | "log") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// The single Runtime-to-Resources application edge for Session outputs.
///
/// It is cloneable so the same operation can decorate direct, durable, and
/// recovered attempts without retaining the whole Host (and creating a
/// SessionCtx -> executor -> Host reference cycle). Terminal release calls the
/// same operation as an idempotent final retry before Sandbox disposal.
#[derive(Clone)]
pub struct ArtifactHarvester {
    sessions: crate::session_realizations::SessionRealizations,
    local_workspace: String,
    publisher:
        std::sync::Arc<dyn awaken_resource_contract::ArtifactPublisher<ResourceOperationFence>>,
}

/// Closed, process-local choice of Artifact source for one harvest attempt.
/// Receipt-only recovery is authorized exclusively by a terminal fence; it is
/// never inferred for an ordinary Run whose live Environment is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactCaptureMode {
    Live,
    ReceiptOnly,
}

#[derive(Debug)]
pub struct HarvestedArtifacts {
    pub receipts: Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
}

impl ArtifactHarvester {
    pub fn new(
        sessions: crate::session_realizations::SessionRealizations,
        local_workspace: String,
        publisher: Arc<dyn awaken_resource_contract::ArtifactPublisher<ResourceOperationFence>>,
    ) -> Self {
        Self {
            sessions,
            local_workspace,
            publisher,
        }
    }

    pub fn publisher(
        &self,
    ) -> Arc<dyn awaken_resource_contract::ArtifactPublisher<ResourceOperationFence>> {
        self.publisher.clone()
    }

    pub fn current_claim(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_dispatch_contract::RunClaim>, RealizationError> {
        self.sessions.dispatch_claim(thread)
    }

    pub async fn harvest(
        &self,
        thread: &str,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        let fence = self
            .current_claim(thread)
            .map_err(artifact_registry_denied)?
            .map(ResourceOperationFence::Run);
        self.harvest_with_fence(thread, fence).await
    }

    pub async fn harvest_with_claim(
        &self,
        thread: &str,
        claim: Option<awaken_dispatch_contract::RunClaim>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence(thread, claim.map(ResourceOperationFence::Run))
            .await
    }

    pub async fn harvest_with_fence(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence_mode(thread, fence, ArtifactCaptureMode::Live)
            .await
    }

    /// Execute one harvest through the sole publish/recovery owner. Callers may
    /// select `ReceiptOnly` only when existing physical cleanup evidence proves
    /// that live output capture must not be attempted.
    pub async fn harvest_with_fence_mode(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
        capture_mode: ArtifactCaptureMode,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence_mode_from_environment(thread, fence, capture_mode, None)
            .await
    }

    /// Use one exact owner-held Environment while retaining the same publisher,
    /// fence validation, idempotency key, and receipt recovery path.
    pub async fn harvest_with_environment_and_fence(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        self.harvest_with_fence_mode_from_environment(
            thread,
            fence,
            ArtifactCaptureMode::Live,
            Some(environment),
        )
        .await
    }

    async fn harvest_with_fence_mode_from_environment(
        &self,
        thread: &str,
        fence: Option<ResourceOperationFence>,
        capture_mode: ArtifactCaptureMode,
        exact_environment: Option<&Arc<crate::session_environment::SessionEnvironment>>,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        let terminal_effect = fence.as_ref().and_then(|fence| match fence {
            ResourceOperationFence::Terminal(effect) => Some(effect),
            ResourceOperationFence::Run(_)
            | ResourceOperationFence::SessionMaterialization(_)
            | ResourceOperationFence::RepositoryPublication { .. }
            | ResourceOperationFence::CheckpointRelease(_) => None,
        });
        if capture_mode == ArtifactCaptureMode::ReceiptOnly && terminal_effect.is_none() {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail: "receipt-only Artifact recovery requires a terminal fence".into(),
            });
        }
        let workspace_owner = terminal_effect
            .map(|effect| effect.command.session_id.as_str())
            .unwrap_or(thread);
        let workspace = self
            .sessions
            .frozen(workspace_owner)
            .map_err(artifact_registry_denied)?
            .workspace
            .clone();
        let workspace = match (workspace, terminal_effect) {
            (Some(workspace), _) => workspace,
            (None, Some(_)) => {
                return Err(ArtifactPublicationError::AuthorityUnavailable {
                    detail: "terminal Artifact recovery has no root Session Workspace projection"
                        .into(),
                });
            }
            (None, None) => self.local_workspace.clone(),
        };
        if capture_mode == ArtifactCaptureMode::ReceiptOnly {
            return self
                .recover_terminal_receipts(
                    thread,
                    workspace,
                    terminal_effect.expect("receipt-only mode requires a terminal effect"),
                )
                .await;
        }
        let projected_environment = match exact_environment {
            Some(environment) => Some(environment.clone()),
            None => self
                .sessions
                .read_realization(thread, |r| r.environment.owner.resident())
                .map_err(artifact_registry_denied)?
                .flatten(),
        };
        let Some(env) = projected_environment else {
            if let Some(effect) = terminal_effect {
                return self
                    .recover_terminal_receipts(thread, workspace, effect)
                    .await;
            }
            return Ok(HarvestedArtifacts {
                receipts: Vec::new(),
            });
        };
        // Read the revision before observing bytes. On CAS conflict a caller
        // must start a fresh harvest, never rebase an old scan onto a new head.
        let current_capture = self
            .publisher
            .capture(&workspace, thread, fence.as_ref())
            .await?;
        if let Some(capture) = &current_capture {
            capture
                .verify_scope(&workspace, thread)
                .map_err(ArtifactPublicationError::from)?;
        }
        let expected_revision = current_capture.map_or(0, |capture| capture.revision);
        let artifacts = env.harvest().capture_artifacts().await.map_err(|error| {
            ArtifactPublicationError::Indeterminate {
                detail: error.to_string(),
            }
        })?;
        let idempotency_scope = terminal_effect.map(|effect| effect.operation_id().to_string());
        let mut receipts = Vec::new();
        for captured in artifacts {
            let artifact = captured.metadata;
            let bytes = captured.bytes;
            let content_id = awaken_resource_contract::content_id(&bytes);
            if content_id != artifact.content_hash || content_id != artifact.id {
                return Err(ArtifactPublicationError::CorruptData {
                    detail: format!("artifact `{}` changed during harvest", artifact.path),
                });
            }
            let logical_path = pc::WorkspaceLayout::outputs_relative(&artifact.path)
                .or_else(|| artifact.path.strip_prefix("/outputs/"))
                .unwrap_or(artifact.path.trim_start_matches('/'))
                .to_string();
            let mime_type = mime_type_for_path(&logical_path).to_string();
            let effect_id = awaken_resource_contract::harvest_idempotency_key(
                thread,
                &logical_path,
                &content_id,
            );
            let publication = awaken_resource_contract::ArtifactPublication {
                effect_id,
                workspace_id: workspace.clone(),
                session_id: thread.to_string(),
                logical_path,
                mime_type,
                content_id,
                bytes,
                idempotency_scope: idempotency_scope.clone(),
                fence: fence.clone(),
            };
            publication.verify()?;
            let receipt = self.publisher.publish(publication.clone()).await?;
            receipt.verify(&publication)?;
            receipts.push(receipt);
        }
        let replacement = awaken_resource_contract::ReplaceArtifactCapture::new(
            workspace,
            thread.into(),
            expected_revision,
            receipts
                .iter()
                .map(|receipt| receipt.record.id.clone())
                .collect(),
        );
        let outcome = self
            .publisher
            .replace_capture(replacement.clone(), fence.as_ref())
            .await?;
        replacement
            .verify_outcome(&outcome)
            .map_err(ArtifactPublicationError::from)?;
        if outcome == awaken_resource_contract::ReplaceArtifactCaptureOutcome::Conflict {
            return Err(ArtifactPublicationError::Conflict {
                detail: "output capture changed; a fresh harvest is required".into(),
            });
        }
        Ok(HarvestedArtifacts { receipts })
    }

    async fn recover_terminal_receipts(
        &self,
        thread: &str,
        workspace: String,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<HarvestedArtifacts, ArtifactPublicationError> {
        let recovery = awaken_resource_contract::ArtifactRecovery {
            workspace_id: workspace,
            session_id: thread.to_string(),
            idempotency_scope: effect.operation_id().to_string(),
            fence: ResourceOperationFence::Terminal(effect.clone()),
        };
        let receipts = self.publisher.recover(recovery).await?;
        Ok(HarvestedArtifacts { receipts })
    }
}
