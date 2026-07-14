use serde::{Deserialize, Serialize};

use crate::agent::content::{ContentBlock, extract_text};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

/// The authoritative minting scheme for run-scoped message ids. Every committed
/// message's id is produced here so the id vocabulary has one owner instead of
/// `format!` templates scattered across the engine (each kind is distinct so ids
/// never collide within a run).
impl Id {
    /// A committed assistant turn: `{run}-assistant-{step}`.
    #[must_use]
    pub fn assistant(run: &crate::agent::run::Id, step: usize) -> Self {
        Self(format!("{}-assistant-{step}", run.0))
    }

    /// The partial text of a `MaxTokens`-truncated turn: `{run}-assistant-{step}-truncated-{nth}`.
    #[must_use]
    pub fn assistant_truncated(run: &crate::agent::run::Id, step: usize, nth: usize) -> Self {
        Self(format!("{}-assistant-{step}-truncated-{nth}", run.0))
    }

    /// The user message asking a truncated turn to continue: `{run}-continuation-{step}-{nth}`.
    #[must_use]
    pub fn continuation(run: &crate::agent::run::Id, step: usize, nth: usize) -> Self {
        Self(format!("{}-continuation-{step}-{nth}", run.0))
    }

    /// A tool-role result addressed to `call_id`: `tool-{call_id}`.
    #[must_use]
    pub fn tool_result(call_id: &str) -> Self {
        Self(format!("tool-{call_id}"))
    }

    /// A steer-feedback continuation message: `{run}-steer-{nth}`.
    #[must_use]
    pub fn steer(run: &crate::agent::run::Id, nth: usize) -> Self {
        Self(format!("{}-steer-{nth}", run.0))
    }

    /// The id prefix shared by a run's steer messages: `{run}-steer-`.
    #[must_use]
    pub fn steer_prefix(run: &crate::agent::run::Id) -> String {
        format!("{}-steer-", run.0)
    }

    /// Whether this id is a steer-feedback message minted for `run`. The id type
    /// owns its own classification so recovering a run's continuation count reads
    /// as a fact over the committed transcript, not a raw string match in the loop.
    #[must_use]
    pub fn is_steer_of(&self, run: &crate::agent::run::Id) -> bool {
        self.0.starts_with(&Self::steer_prefix(run))
    }

    /// A resume-input message replying to `call_id`: `resume-input-{call_id}`.
    #[must_use]
    pub fn resume_input(call_id: &str) -> Self {
        Self(format!("resume-input-{call_id}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in a thread. Content is multimodal: a list of content blocks,
/// never a bare string, so an image rides alongside text without a schema change.
/// Not `Eq` — a `ToolUse` block carries untyped JSON arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A plain-text message — the common case, `vec![ContentBlock::text(..)]`.
    pub fn text(id: Id, role: Role, text: impl Into<String>) -> Self {
        Self {
            id,
            role,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A message from a ready-made block list — the multimodal case, where a turn
    /// interleaves text with an image (or other media) block.
    pub fn new(id: Id, role: Role, content: Vec<ContentBlock>) -> Self {
        Self { id, role, content }
    }

    /// The concatenated text of this message's `Text` blocks, ignoring media.
    pub fn text_content(&self) -> String {
        extract_text(&self.content)
    }
}
