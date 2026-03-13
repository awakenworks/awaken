use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tirea_state::State;

/// A compact boundary marking that all messages up through a given message ID
/// have been logically replaced by a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactBoundary {
    /// ID of the last message covered by this boundary.
    pub covers_through_message_id: String,
    /// Pre-computed summary text that replaces the covered messages.
    pub summary: String,
    /// Estimated token count of the original messages that were summarized.
    pub original_token_count: usize,
    /// Timestamp (ms since epoch) when this boundary was created.
    pub created_at_ms: u64,
}

/// A reference to a large artifact whose content is replaced with a
/// lightweight compact view during inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Message ID containing the artifact, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Tool call ID if the artifact is a tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Human-readable label for the compact view.
    pub label: String,
    /// Compact preview shown to the model during inference.
    pub summary: String,
    /// Original content size in characters.
    pub original_size: usize,
    /// Estimated original token count.
    pub original_token_count: usize,
}

/// Thread-scoped state persisting compact boundaries and artifact references.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "__context", action = "ContextAction", scope = "thread")]
pub struct ContextState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundaries: Vec<CompactBoundary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<ArtifactRef>,
}

/// Actions that modify [`ContextState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextAction {
    /// Add a new compact boundary. Boundaries are cumulative; the latest one
    /// supersedes any earlier boundary.
    AddBoundary(CompactBoundary),
    /// Register an artifact reference for compact-view substitution.
    AddArtifact(ArtifactRef),
    /// Remove artifact refs that are fully covered by compaction.
    PruneArtifacts {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        message_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_call_ids: Vec<String>,
    },
}

impl ContextState {
    pub(super) fn reduce(&mut self, action: ContextAction) {
        match action {
            ContextAction::AddBoundary(boundary) => {
                self.boundaries.clear();
                self.boundaries.push(boundary);
            }
            ContextAction::AddArtifact(artifact) => {
                if let Some(existing) = self
                    .artifact_refs
                    .iter_mut()
                    .find(|existing| artifact_identity_matches(existing, &artifact))
                {
                    *existing = artifact;
                } else {
                    self.artifact_refs.push(artifact);
                }
            }
            ContextAction::PruneArtifacts {
                message_ids,
                tool_call_ids,
            } => {
                if message_ids.is_empty() && tool_call_ids.is_empty() {
                    return;
                }
                let message_ids: HashSet<String> = message_ids.into_iter().collect();
                let tool_call_ids: HashSet<String> = tool_call_ids.into_iter().collect();
                self.artifact_refs.retain(|artifact| {
                    let message_match = artifact
                        .message_id
                        .as_ref()
                        .is_some_and(|id| message_ids.contains(id));
                    let tool_match = artifact
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| tool_call_ids.contains(id));
                    !(message_match || tool_match)
                });
            }
        }
    }

    pub(super) fn latest_boundary(&self) -> Option<&CompactBoundary> {
        self.boundaries.last()
    }
}

fn artifact_identity_matches(existing: &ArtifactRef, candidate: &ArtifactRef) -> bool {
    existing.message_id.is_some() && existing.message_id == candidate.message_id
        || existing.tool_call_id.is_some() && existing.tool_call_id == candidate.tool_call_id
}
