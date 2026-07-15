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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::run::Id as RunId;

    #[test]
    fn id_minting_schemes_are_distinct_and_well_formed() {
        let run = RunId("r".into());
        assert_eq!(Id::assistant(&run, 2).0, "r-assistant-2");
        assert_eq!(
            Id::assistant_truncated(&run, 2, 1).0,
            "r-assistant-2-truncated-1"
        );
        assert_eq!(Id::continuation(&run, 2, 0).0, "r-continuation-2-0");
        assert_eq!(Id::tool_result("call9").0, "tool-call9");
        assert_eq!(Id::steer(&run, 3).0, "r-steer-3");
        assert_eq!(Id::steer_prefix(&run), "r-steer-");
        assert_eq!(Id::resume_input("call9").0, "resume-input-call9");
    }

    #[test]
    fn is_steer_of_matches_only_its_own_runs_steer_ids() {
        let run = RunId("r".into());
        let other = RunId("r2".into());
        assert!(Id::steer(&run, 0).is_steer_of(&run));
        // A steer id for `r2` must not read as a steer of `r` (prefix `r-steer-`
        // vs `r2-steer-`), so the prefix test does not false-positive.
        assert!(!Id::steer(&other, 0).is_steer_of(&run));
        // A non-steer id (an assistant turn) is never a steer.
        assert!(!Id::assistant(&run, 0).is_steer_of(&run));
    }

    #[test]
    fn text_content_delegates_to_extract_text() {
        let msg = Message::new(
            Id("m".into()),
            Role::Assistant,
            vec![ContentBlock::text("x"), ContentBlock::text("y")],
        );
        assert_eq!(msg.text_content(), "xy");
    }
}
