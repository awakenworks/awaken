use serde::{Deserialize, Serialize};

use crate::agent::content::{ContentBlock, extract_text};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

/// The authoritative minting scheme for run-scoped message ids. Every committed
/// message's id is produced here so the id vocabulary has one owner instead of
/// `format!` templates scattered across the engine (each kind is distinct so ids
/// never collide within a run).
impl Id {
    const AGENT_THREAD_REPORT_PREFIX: &'static str = "agent-thread-report-";
    const SESSION_EVENT_INPUT_PREFIX: &'static str = "session-event-input-v1-";
    const SESSION_SYSTEM_PREFIX: &'static str = "session-system-v1-";

    /// Prefix shared by committed assistant Step messages for `run`.
    #[must_use]
    pub fn assistant_prefix(run: &crate::agent::run::Id) -> String {
        format!("{}-assistant-", run.0)
    }

    /// A committed assistant Step message: `{run}-assistant-{step}`.
    #[must_use]
    pub fn assistant(run: &crate::agent::run::Id, step: usize) -> Self {
        Self(format!("{}{step}", Self::assistant_prefix(run)))
    }

    /// Recover the exact Step of a full assistant message for `run`. Truncated
    /// partials deliberately return `None`; they do not consume another step.
    #[must_use]
    pub fn assistant_step_of(&self, run: &crate::agent::run::Id) -> Option<usize> {
        self.0
            .strip_prefix(&Self::assistant_prefix(run))?
            .parse()
            .ok()
    }

    /// Recover the assistant step and response ordinal of a committed
    /// `MaxTokens` partial for `run`. The response ordinal is the `nth` encoded
    /// by [`Self::assistant_truncated`] and is the same coordinate carried by
    /// live model deltas before this Message commits.
    #[must_use]
    pub fn assistant_truncated_response_of(
        &self,
        run: &crate::agent::run::Id,
    ) -> Option<(usize, usize)> {
        let suffix = self.0.strip_prefix(&Self::assistant_prefix(run))?;
        let (step, response) = suffix.split_once("-truncated-")?;
        Some((step.parse().ok()?, response.parse().ok()?))
    }

    /// The partial text of a `MaxTokens`-truncated Step response:
    /// `{run}-assistant-{step}-truncated-{nth}`.
    #[must_use]
    pub fn assistant_truncated(run: &crate::agent::run::Id, step: usize, nth: usize) -> Self {
        Self(format!("{}-assistant-{step}-truncated-{nth}", run.0))
    }

    /// The user message asking a truncated Step response to continue:
    /// `{run}-continuation-{step}-{nth}`.
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

    /// Internal input that delivers one child Thread's committed report to its
    /// coordinator in a later Run.
    ///
    /// The typed mint/classifier is the provenance owner. Protocol projectors
    /// can suppress this transport message from ordinary `user.message` output
    /// without guessing at human-readable content.
    #[must_use]
    pub fn agent_thread_report(child_run_id: &crate::agent::run::Id) -> Self {
        Self(format!(
            "{}{}",
            Self::AGENT_THREAD_REPORT_PREFIX,
            crate::stable_fingerprint(&("agent-thread-report-v1", child_run_id.0.as_str()))
        ))
    }

    /// Whether this id belongs to the internal child-report identity family.
    #[must_use]
    pub fn is_agent_thread_report(&self) -> bool {
        self.0.starts_with(Self::AGENT_THREAD_REPORT_PREFIX)
    }

    /// Bind an internal report message to the exact child Run that produced it.
    #[must_use]
    pub fn is_agent_thread_report_of(&self, child_run_id: &crate::agent::run::Id) -> bool {
        self == &Self::agent_thread_report(child_run_id)
    }

    /// Stable identity for one Session event input. The opaque operation id is
    /// retained behind a length-delimited Session coordinate so retries mint the
    /// same id without colliding with another Session's operation.
    #[must_use]
    pub fn session_event_input(session_id: &str, operation_id: &str) -> Self {
        Self(format!(
            "{}{session_len}:{session_id}{operation_id}",
            Self::SESSION_EVENT_INPUT_PREFIX,
            session_len = session_id.len(),
        ))
    }

    /// Stable identity for one Session Event system message. The opaque
    /// operation id is retained behind the same length-delimited Session
    /// coordinate used by [`Self::session_event_input`].
    #[must_use]
    pub fn session_system(session_id: &str, operation_id: &str) -> Self {
        Self(format!(
            "{}{session_len}:{session_id}{operation_id}",
            Self::SESSION_SYSTEM_PREFIX,
            session_len = session_id.len(),
        ))
    }
}

/// Select the next canonical assistant Step from one committed transcript.
///
/// External executors and the native resume path share this classifier so a
/// committed response has one Run/Step identity vocabulary regardless of which
/// execution backend produced it. Truncated partials do not consume a new Step.
#[must_use]
pub fn next_assistant_step(messages: &[Message], run_id: &crate::agent::run::Id) -> usize {
    messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter_map(|message| message.id.assistant_step_of(run_id))
        .max()
        .map_or(0, |step| step + 1)
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

    /// A message from a ready-made block list — the multimodal case, where a message
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

    /// Message-id cause/effect rules: canonical final ids recover only a Step;
    /// canonical truncated ids recover exactly `(Step,response)` and never pass
    /// as final ids; identical Session/operation coordinates mint the same
    /// length-delimited id while a different operation mints a distinct id.
    /// Decision rows: final=>Step/none; truncated=>none/(Step,response);
    /// same Session coordinate=>same id; different operation=>different id.
    /// Remaining assertions cover distinct mint families.
    /// Constraints/invariants: every minted identifier is deterministic,
    /// namespace-scoped, and cannot be decoded as a sibling identifier family.
    #[test]
    fn id_minting_schemes_are_distinct_and_well_formed() {
        let run = RunId("r".into());
        assert_eq!(Id::assistant(&run, 2).0, "r-assistant-2");
        assert_eq!(
            Id::assistant_step_of(&Id::assistant(&run, 2), &run),
            Some(2)
        );
        assert_eq!(
            Id::assistant_step_of(&Id::assistant_truncated(&run, 2, 1), &run),
            None
        );
        let system = Id::session_system("session-1", "operation-1");
        assert_eq!(system.0, "session-system-v1-9:session-1operation-1");
        assert_eq!(system, Id::session_system("session-1", "operation-1"));
        assert_ne!(system, Id::session_system("session-1", "operation-2"));
        let event_input = Id::session_event_input("session-1", "batch-4:event-2");
        assert_eq!(
            event_input.0,
            "session-event-input-v1-9:session-1batch-4:event-2"
        );
        assert_eq!(
            event_input,
            Id::session_event_input("session-1", "batch-4:event-2")
        );
        assert_ne!(
            event_input,
            Id::session_event_input("session-1", "batch-4:event-3")
        );
        assert_eq!(
            Id::assistant_truncated_response_of(&Id::assistant_truncated(&run, 2, 1), &run),
            Some((2, 1))
        );
        assert_eq!(
            Id::assistant_truncated_response_of(&Id::assistant(&run, 2), &run),
            None
        );
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
        // Test design — Causes: a canonical steer id is checked against its
        // owning Run and against a different Run. Effects: only the owner
        // matches. Constraints/invariants: steer correlation is exact and
        // cannot cross Run boundaries. Decision rule S1: owner=>true;
        // non-owner=>false.
        let run = RunId("r".into());
        let other = RunId("r2".into());
        assert!(Id::steer(&run, 0).is_steer_of(&run));
        // A steer id for `r2` must not read as a steer of `r` (prefix `r-steer-`
        // vs `r2-steer-`), so the prefix test does not false-positive.
        assert!(!Id::steer(&other, 0).is_steer_of(&run));
        // A non-steer id (an assistant Step message) is never a steer.
        assert!(!Id::assistant(&run, 0).is_steer_of(&run));
    }

    #[test]
    fn agent_thread_report_identity_is_stable_distinct_and_typed() {
        // Cause/effect graph: same child Run -> same report id (retry);
        // different child Run -> different id (no cross-report dedupe);
        // ordinary input -> not internal provenance. These three rules are the
        // complete classifier decision table used by protocol projection.
        // Constraints/invariants: report identity is deterministic and scoped
        // to exactly one child Run; ordinary input never becomes provenance.
        let child = RunId("child/run:1".into());
        let other = RunId("child/run:2".into());
        let report = Id::agent_thread_report(&child);
        assert_eq!(report, Id::agent_thread_report(&child), "stable retry");
        assert_ne!(report, Id::agent_thread_report(&other), "distinct child");
        assert!(report.is_agent_thread_report());
        assert!(report.is_agent_thread_report_of(&child));
        assert!(!report.is_agent_thread_report_of(&other));
        assert!(!Id("ordinary-user-input".into()).is_agent_thread_report());
    }

    #[test]
    fn next_assistant_step_uses_only_complete_messages_from_the_selected_run() {
        // Cause/effect graph: C1 selected Run has no/full canonical assistant
        // Messages; C2 transcript also contains another Run, a truncated
        // partial, or a non-assistant role carrying an assistant-shaped id.
        // Effects: E1 no complete selected response starts at Step 0; E2 complete
        // selected Steps advance to max+1; E3 C2 never advances the coordinate.
        //
        // | Rule | selected complete Steps | distracting facts | Effect |
        // | N1   | none                    | any               | E1     |
        // | N2   | 0,2                     | none              | E2=3   |
        // | N3   | 0,2                     | C2 present        | E2+E3  |
        // Constraints/invariants: selection is Run-scoped and monotonic over
        // complete assistant messages; foreign/incomplete rows cannot advance it.
        let run = RunId("selected".into());
        let other = RunId("other".into());
        assert_eq!(next_assistant_step(&[], &run), 0, "N1/E1");

        let messages = vec![
            Message::text(Id::assistant(&run, 0), Role::Assistant, "zero"),
            Message::text(Id::assistant(&run, 2), Role::Assistant, "two"),
            Message::text(
                Id::assistant_truncated(&run, 9, 0),
                Role::Assistant,
                "partial",
            ),
            Message::text(Id::assistant(&other, 8), Role::Assistant, "other"),
            Message::text(Id::assistant(&run, 12), Role::User, "malformed"),
        ];
        assert_eq!(next_assistant_step(&messages[..2], &run), 3, "N2/E2");
        assert_eq!(next_assistant_step(&messages, &run), 3, "N3/E2+E3");
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
