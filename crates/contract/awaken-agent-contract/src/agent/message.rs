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
        let (owner, step, response) = self.assistant_coordinates()?;
        (owner == run.0 && response.is_none()).then_some(step)
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
        let (owner, step, response) = self.assistant_coordinates()?;
        (owner == run.0).then_some((step, response?))
    }

    /// Decode the owning Run, Step, and optional truncated response ordinal
    /// without searching known Runs or allocating a prefix for each candidate.
    /// The rightmost assistant delimiter owns the numeric suffix: a Run may
    /// itself contain this delimiter, whereas a Step/response cannot. This is
    /// the same decoder used by the Run-scoped classifiers above.
    #[must_use]
    pub fn assistant_coordinates(&self) -> Option<(&str, usize, Option<usize>)> {
        let (owner, suffix) = self.0.rsplit_once("-assistant-")?;
        if let Some((step, response)) = suffix.split_once("-truncated-") {
            Some((owner, step.parse().ok()?, Some(response.parse().ok()?)))
        } else {
            Some((owner, suffix.parse().ok()?, None))
        }
    }

    /// The partial text of a `MaxTokens`-truncated Step response:
    /// `{run}-assistant-{step}-truncated-{nth}`.
    #[must_use]
    pub fn assistant_truncated(run: &crate::agent::run::Id, step: usize, nth: usize) -> Self {
        Self(format!("{}-truncated-{nth}", Self::assistant(run, step).0))
    }

    /// The user message asking a truncated Step response to continue:
    /// `{run}-continuation-{step}-{nth}`.
    #[must_use]
    pub fn continuation(run: &crate::agent::run::Id, step: usize, nth: usize) -> Self {
        Self(format!("{}-continuation-{step}-{nth}", run.0))
    }

    /// Whether this is an internal truncated-response continuation for `run`.
    /// Public history can exclude feedback by identity, without inspecting prose.
    #[must_use]
    pub fn is_continuation_of(&self, run: &crate::agent::run::Id) -> bool {
        self.0
            .strip_prefix(&format!("{}-continuation-", run.0))
            .and_then(|suffix| suffix.split_once('-'))
            .is_some_and(|(step, nth)| {
                step.parse::<usize>().is_ok() && nth.parse::<usize>().is_ok()
            })
    }

    /// Legacy committed tool-role result identity. New Runtime results use the
    /// owning durable ToolBatch coordinate; keep this reader for old commits.
    #[must_use]
    pub fn tool_result(call_id: &str) -> Self {
        Self(format!("tool-{call_id}"))
    }

    /// One result of a call in a durable ToolBatch. The batch id is already
    /// Run/Step scoped; its byte length keeps delimiter-bearing provider call
    /// ids from aliasing another batch/call pair. Provider correlation content
    /// remains the original call id and is never rewritten to this message id.
    #[must_use]
    pub fn tool_result_for_batch(batch_id: &str, call_id: &str) -> Self {
        Self(format!(
            "batch-tool-result-v2:{}:{batch_id}:{call_id}",
            batch_id.len()
        ))
    }

    /// Decode the Run, Step and provider call of a batch-scoped Runtime result.
    /// Legacy identities have no such coordinate. This is the canonical reader
    /// shared by call recognition and committed occurrence attribution; a
    /// decoded identity does not establish that its ToolBatch was committed.
    #[must_use]
    pub fn tool_result_coordinates(&self) -> Option<(&str, usize, &str)> {
        let encoded = self.0.strip_prefix("batch-tool-result-v2:")?;
        let (batch_len, rest) = encoded.split_once(':')?;
        let batch_len = batch_len.parse::<usize>().ok()?;
        let batch_id = rest.get(..batch_len)?;
        let (run, step) = batch_id.strip_prefix("tool-batch:")?.rsplit_once(':')?;
        let number = step.parse::<usize>().ok()?;
        if run.is_empty() || number.to_string() != step {
            return None;
        }
        let call_id = rest.get(batch_len..)?.strip_prefix(':')?;
        Some((run, number, call_id))
    }

    /// Recognize the exact provider call addressed by a Runtime result,
    /// including legacy committed results. A different call or SDK result id
    /// does not acquire Runtime-owned receipt authority by its content alone.
    #[must_use]
    pub fn is_tool_result_for_call(&self, call_id: &str) -> bool {
        self == &Self::tool_result(call_id)
            || self
                .tool_result_coordinates()
                .is_some_and(|(_, _, result_call)| result_call == call_id)
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

    /// Internal model-authored task input for a delegated Thread. Preserve the
    /// existing message identity so recovery and command equivalence stay exact.
    #[must_use]
    pub fn agent_thread_input(child_run_id: &crate::agent::run::Id) -> Self {
        Self(format!("{}-input", child_run_id.0))
    }

    /// Internal advisor request following the copied primary transcript.
    #[must_use]
    pub fn agent_advisor_input(child_run_id: &crate::agent::run::Id) -> Self {
        Self(format!("{}-advisor-input", child_run_id.0))
    }

    /// Classify the existing internal task-input family by its length-delimited
    /// child Run identity. A normal user message does not acquire this identity
    /// from its role or prose, and no Web policy is interpreted here.
    #[must_use]
    pub fn is_agent_thread_input(&self) -> bool {
        let Some(run) = self
            .0
            .strip_suffix("-advisor-input")
            .or_else(|| self.0.strip_suffix("-input"))
        else {
            return false;
        };
        let Some(encoded) = run.strip_prefix("child-run:") else {
            return false;
        };
        let Some((length, delegation)) = encoded.split_once(':') else {
            return false;
        };
        length
            .parse::<usize>()
            .is_ok_and(|value| value.to_string() == length && value == delegation.len())
            && delegation.starts_with("delegation:")
    }

    /// Stable identity for one Session event input. The opaque operation id is
    /// retained behind a length-delimited Session coordinate so retries mint the
    /// same id without colliding with another Session's operation.
    #[must_use]
    pub fn session_event_input(session_id: &str, operation_id: &str) -> Self {
        Self::session_scoped(Self::SESSION_EVENT_INPUT_PREFIX, session_id, operation_id)
    }

    /// Stable identity for one Session Event system message. The opaque
    /// operation id is retained behind the same length-delimited Session
    /// coordinate used by [`Self::session_event_input`].
    #[must_use]
    pub fn session_system(session_id: &str, operation_id: &str) -> Self {
        Self::session_scoped(Self::SESSION_SYSTEM_PREFIX, session_id, operation_id)
    }

    fn session_scoped(prefix: &str, session_id: &str, operation_id: &str) -> Self {
        Self(format!(
            "{prefix}{}:{session_id}{operation_id}",
            session_id.len()
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

    #[test]
    fn internal_agent_input_identity_preserves_bytes_and_requires_canonical_child_coordinates() {
        // I1 task/advisor deterministic identities keep existing bytes; I2
        // normal user inputs, malformed lengths, noncanonical numeric lengths
        // and a suffix peer do not become internal provenance by role/prose.
        let child = crate::agent::delegation::DelegationOrigin::root(
            crate::agent::run::Id("parent/父".into()),
            "call:1",
        )
        .child_run_id();
        let task = Id::agent_thread_input(&child);
        let advisor = Id::agent_advisor_input(&child);
        assert_eq!(task.0, format!("{}-input", child.0));
        assert_eq!(advisor.0, format!("{}-advisor-input", child.0));
        assert!(task.is_agent_thread_input());
        assert!(advisor.is_agent_thread_input());
        for value in [
            "user-input",
            "child-run:1:delegation:parent-input",
            "child-run:011:delegation:-input",
            "child-run:10:delegation-input",
        ] {
            assert!(!Id(value.into()).is_agent_thread_input(), "{value}");
        }
        assert!(!Id(format!("{}-peer", task.0)).is_agent_thread_input());
    }
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
    fn tool_result_identity_is_batch_scoped_and_legacy_readable() {
        // Causes: C1 same batch and provider call (including cold replay); C2
        // another Run/Step batch reuses the provider call; C3 another call in
        // the same batch; C4 delimiter-bearing batch/call pairs; C5 a legacy
        // committed result; C6 forged/noncanonical v2 or SDK result identity.
        // Effects: E1 stable ID; E2 distinct IDs; E3 exact current and legacy
        // reads without classifying a different call; E4 reject forged ids.
        //
        // | Rule | Batch | Call | Effect |
        // | R1 | same | same | E1,E3 |
        // | R2 | different | same | E2,E3 |
        // | R3 | same | different | E2,E3 |
        // | R4 | delimiter collision candidate | different | E2 |
        // | R5 | legacy committed | same/different | E3 |
        // | R6 | noncanonical/SDK | same | E4 |
        let first = Id::tool_result_for_batch("tool-batch:run-a:0", "voice-final");
        assert_eq!(
            first,
            Id::tool_result_for_batch("tool-batch:run-a:0", "voice-final"),
            "R1/E1"
        );
        assert_ne!(
            first,
            Id::tool_result_for_batch("tool-batch:run-a:1", "voice-final"),
            "R2/E2"
        );
        assert_ne!(
            first,
            Id::tool_result_for_batch("tool-batch:run-a:0", "other"),
            "R3/E2"
        );
        assert_ne!(
            Id::tool_result_for_batch("tool-batch:a:0:b", "c"),
            Id::tool_result_for_batch("tool-batch:a:0", "b:c"),
            "R4/E2"
        );
        assert!(first.is_tool_result_for_call("voice-final"), "R1/E3");
        assert!(!first.is_tool_result_for_call("voice"), "R3/E3");
        assert!(
            Id::tool_result("voice-final").is_tool_result_for_call("voice-final"),
            "R5/E3"
        );
        assert!(
            !Id::tool_result("voice-final").is_tool_result_for_call("voice"),
            "R5/E3"
        );
        for forged in [
            Id::tool_result_for_batch("not-a-batch", "voice-final"),
            Id::tool_result_for_batch("tool-batch::0", "voice-final"),
            Id::tool_result_for_batch("tool-batch:run:x", "voice-final"),
            Id::tool_result_for_batch("tool-batch:run:01", "voice-final"),
            Id("batch-tool-result-v2:100:tool-batch:run:0:voice-final".into()),
            Id("sdk-result-voice-final".into()),
        ] {
            assert!(
                !forged.is_tool_result_for_call("voice-final"),
                "R6 forged or SDK-looking ID must not grant Runtime receipt authority: {forged:?}"
            );
        }
    }

    #[test]
    fn tool_result_coordinate_reader_preserves_exact_batch_scope() {
        // Causes: C1 canonical batch Run/Step/call, including delimiters and
        // Unicode; C2 zero/max Step; C3 legacy result; C4 malformed length,
        // UTF-8 boundary, Run, Step or separator. Effects: E1 C1+C2 returns
        // its exact borrowed scope and retains call recognition; E2 C3+C4
        // has no batch scope. R1=C1+C2=>E1; R2=C3=>E2 with the original
        // legacy recognizer still true; R3=C4=>E2 and recognizer false.
        for (run, step, call) in [
            ("run", 0, "same"),
            ("运行:assistant-2", 7, "调用:final"),
            ("run:with:delimiters", usize::MAX, ""),
        ] {
            let id = Id::tool_result_for_batch(&format!("tool-batch:{run}:{step}"), call);
            assert_eq!(id.tool_result_coordinates(), Some((run, step, call)), "R1");
            assert!(id.is_tool_result_for_call(call), "R1 original recognition");
            assert!(!id.is_tool_result_for_call("different"), "R1 exact call");
        }
        let legacy = Id::tool_result("same");
        assert_eq!(legacy.tool_result_coordinates(), None, "R2");
        assert!(legacy.is_tool_result_for_call("same"), "R2");
        for id in [
            "batch-tool-result-v2:999:tool-batch:run:0:same",
            "batch-tool-result-v2:12:tool-batch:运:0:same",
            "batch-tool-result-v2:13:tool-batch::0:same",
            "batch-tool-result-v2:17:tool-batch:run:01:same",
            "batch-tool-result-v2:17:tool-batch:run:+1:same",
            "batch-tool-result-v2:17:tool-batch:run:-1:same",
            "batch-tool-result-v2:16:tool-batch:run:0!same",
        ]
        .map(|value| Id(value.into()))
        {
            assert_eq!(id.tool_result_coordinates(), None, "R3 {id:?}");
            assert!(!id.is_tool_result_for_call("same"), "R3 {id:?}");
        }
    }

    #[test]
    fn assistant_coordinate_decoder_preserves_run_scoped_classification() {
        // Causes: C1 Run is empty/plain/Unicode/contains either delimiter;
        // C2 suffix is full/truncated/invalid; C3 queried Run is owner/other;
        // C4 integer is zero/leading-zero/plus/max/overflow/negative/empty.
        // Rules D1=C1+valid C2+C4 => borrowed exact owner and coordinates;
        // D2=D1+owner => exactly one scoped classifier accepts;
        // D3=D1+other => neither accepts; D4=invalid C2/C4 => neither accepts.
        // The independent prefix-and-numeric oracle freezes the previous
        // accepted language, including noncanonical but previously readable
        // integers. Rightmost delimiter selection cannot alias a shorter Run.
        let owners = ["", "r", "r-assistant-2", "r-truncated-0", "运行-assistant-"];
        let numbers = vec![
            "0".to_string(),
            "02".into(),
            "+2".into(),
            "".into(),
            "-1".into(),
            " 2".into(),
            "2 ".into(),
            "x".into(),
            usize::MAX.to_string(),
            format!("{}0", usize::MAX),
        ];
        for owner in owners {
            let run = RunId(owner.into());
            for step in &numbers {
                let mut suffixes = vec![step.clone()];
                suffixes.extend(
                    numbers
                        .iter()
                        .map(|response| format!("{step}-truncated-{response}")),
                );
                for suffix in suffixes {
                    let id = Id(format!("{owner}-assistant-{suffix}"));
                    for candidate in owners {
                        let candidate = RunId(candidate.into());
                        let old_suffix = id.0.strip_prefix(&format!("{}-assistant-", candidate.0));
                        let full = old_suffix.and_then(|value| value.parse::<usize>().ok());
                        let partial = old_suffix.and_then(|value| {
                            let (step, response) = value.split_once("-truncated-")?;
                            Some((step.parse::<usize>().ok()?, response.parse::<usize>().ok()?))
                        });
                        assert_eq!(
                            id.assistant_step_of(&candidate),
                            full,
                            "D2-D4 {id:?}/{candidate:?}"
                        );
                        assert_eq!(
                            id.assistant_truncated_response_of(&candidate),
                            partial,
                            "D2-D4 {id:?}/{candidate:?}"
                        );
                    }
                    if let Some((decoded_owner, step, response)) = id.assistant_coordinates() {
                        assert_eq!(decoded_owner, owner, "D1 longest exact owner");
                        assert_eq!(decoded_owner.as_ptr(), id.0.as_ptr(), "D1 borrowed source");
                        match response {
                            None => assert_eq!(id.assistant_step_of(&run), Some(step)),
                            Some(response) => assert_eq!(
                                id.assistant_truncated_response_of(&run),
                                Some((step, response))
                            ),
                        }
                    }
                }
            }
        }
        for invalid in [
            "ordinary",
            "r-assistant",
            "r-assistant--truncated-",
            "r-assistant-1-truncated-0-truncated-1",
        ] {
            assert_eq!(
                Id(invalid.into()).assistant_coordinates(),
                None,
                "D4 {invalid}"
            );
        }
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
    fn continuation_provenance_is_exact_and_requires_both_numeric_coordinates() {
        // C1 canonical continuation; C2 wrong Run; C3 ordinary or malformed
        // identity. R1 C1+owner => internal feedback; R2 C2 or C3 => not
        // feedback. No message text participates in the classification.
        let run = RunId("r-with-delimiters".into());
        assert!(Id::continuation(&run, 2, 3).is_continuation_of(&run));
        assert!(!Id::continuation(&run, 2, 3).is_continuation_of(&RunId("r".into())));
        for value in [
            "ordinary",
            "r-with-delimiters-continuation-2",
            "r-with-delimiters-continuation-x-3",
            "r-with-delimiters-continuation-2-3-x",
        ] {
            assert!(!Id(value.into()).is_continuation_of(&run), "R2 {value}");
        }
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
