// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: which committed Messages of a coordinated child are folded, withheld,
// @formal reason: consumed as its report, or folded into one Advisor consultation result; moved from the
// @formal reason: Managed transcript projector. Dynamic path: the report classifier of the Session
// @formal reason: coordination contract and the committed lifecycle decide; the protocol encodes. Evidence:
// @formal reason: session.multiagent.thread_authority and the managed transcript projection tests.
//! Transcript plans: which committed Messages a projection folds, holds back
//! or consumes as a cross-Thread report, and what those reports say.
//!
//! Consumer: the protocol adapter's root and child transcript projectors,
//! which execute each step against their published tool events.

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::{RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_execution_contract::{session_agent_report_messages, session_agent_report_text};

use crate::ProjectionError;
use crate::transcript::assistant_response_coordinates;

/// One step of a coordinated child's transcript projection, in Message order.
#[derive(Debug)]
pub enum ChildTranscriptStep<'a> {
    /// A report Message of `run_id`: consumed; the first one of each Run
    /// publishes the Run's non-empty report text.
    Report {
        message: &'a Message,
        run_id: String,
        text: Option<String>,
    },
    /// An ordinary Message whose classified content is folded.
    Fold(&'a Message),
}

/// The transcript projection of one coordinated child.
#[derive(Debug)]
pub enum ChildTranscriptPlan<'a> {
    /// An Advisor consultation publishes at most one result for its Run.
    Advisor {
        /// Messages consumed without a public event of their own.
        consume: Vec<&'a Message>,
        /// The latest Run's report content, when a report is not yet consumed.
        report: Option<(RunId, Vec<ContentBlock>)>,
    },
    Ordinary {
        steps: Vec<ChildTranscriptStep<'a>>,
        coordinates: HashMap<String, (String, usize, usize)>,
    },
}

/// The committed facts one child transcript plan reads.
pub struct ChildTranscript<'a> {
    pub thread_id: &'a str,
    pub advisor: bool,
    pub messages: &'a [Message],
    /// This child's accepted lifecycle suffix.
    pub lifecycle: &'a [RunLifecycleEvent],
    pub latest_run_id: Option<&'a RunId>,
    pub latest_run_state: Option<&'a RunState>,
}

/// Plan one coordinated child's transcript projection over the Messages not
/// yet consumed (`consumed` answers by Message id).
///
/// An Advisor waits until its Run ends: a failed or cancelled consultation
/// consumes everything without a public receive; a successful one publishes
/// only its latest Run's report, folding provider chunks into one result. An
/// ordinary child publishes each completed Run's non-empty classifier report
/// once and folds everything else, except an assistant response not yet
/// proven ordinary, which stays unconsumed until its Run's boundary tells
/// whether it is the report.
pub fn plan_child_transcript<'a>(
    input: &ChildTranscript<'a>,
    consumed: impl Fn(&str) -> bool,
) -> Result<ChildTranscriptPlan<'a>, ProjectionError> {
    let run_ids = input
        .lifecycle
        .iter()
        .map(|event| event.run_id.clone())
        .chain(input.latest_run_id.cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let coordinates = assistant_response_coordinates(input.messages, &run_ids);
    let new_messages = input
        .messages
        .iter()
        .filter(|message| !consumed(&message.id.0))
        .collect::<Vec<_>>();
    if input.advisor {
        match input.latest_run_state {
            Some(RunState::Ended(EndCause::NaturalEnd | EndCause::MaxSteps)) => {}
            Some(RunState::Ended(_)) => {
                return Ok(ChildTranscriptPlan::Advisor {
                    consume: new_messages,
                    report: None,
                });
            }
            Some(RunState::Running | RunState::Awaiting) | None => {
                return Ok(ChildTranscriptPlan::Advisor {
                    consume: Vec::new(),
                    report: None,
                });
            }
        }
        let latest_run_id = input.latest_run_id.ok_or_else(|| {
            ProjectionError::corrupt("terminal advisor recovery snapshot has no latest Run id")
        })?;
        let report_messages = session_agent_report_messages(input.messages, latest_run_id);
        let has_unprojected_report = report_messages
            .iter()
            .any(|report| new_messages.iter().any(|message| message.id == report.id));
        let report = has_unprojected_report.then(|| {
            (
                latest_run_id.clone(),
                report_messages
                    .iter()
                    .flat_map(|message| message.content.iter().cloned())
                    .collect(),
            )
        });
        return Ok(ChildTranscriptPlan::Advisor {
            consume: new_messages,
            report,
        });
    }

    let ordinary_response_steps = input
        .messages
        .iter()
        .filter(|message| {
            message.role == Role::Assistant
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        })
        .filter_map(|message| {
            coordinates
                .get(&message.id.0)
                .map(|(run_id, step, _)| (run_id.clone(), *step))
        })
        .collect::<HashSet<_>>();
    // The Session coordination contract is the sole report classifier used by
    // settlement and this projector. A normally completed child Run
    // contributes one cross-Thread report only when its classifier-selected
    // text is non-empty; a tool-only terminal Message falls through to
    // occurrence projection so every classified call precedes its result.
    // Failed and Cancelled Runs settle directly and never own a report.
    let mut report_run_by_message = HashMap::<String, String>::new();
    let mut report_text_by_run = HashMap::<String, String>::new();
    for terminal in input
        .lifecycle
        .iter()
        .filter(|event| event.kind == RunLifecycleEventKind::Completed)
    {
        let report_text = session_agent_report_text(input.messages, &terminal.run_id);
        if report_text.is_empty() {
            continue;
        }
        let report_messages = session_agent_report_messages(input.messages, &terminal.run_id);
        if report_messages.is_empty() {
            continue;
        }
        for message in report_messages {
            report_run_by_message.insert(message.id.0.clone(), terminal.run_id.0.clone());
        }
        report_text_by_run.insert(terminal.run_id.0.clone(), report_text);
    }
    let mut steps = Vec::new();
    let mut projected_reports = HashSet::new();
    for message in new_messages {
        if let Some(report_run_id) = report_run_by_message.get(&message.id.0) {
            let text = if projected_reports.insert(report_run_id.clone()) {
                report_text_by_run
                    .get(report_run_id)
                    .filter(|text| !text.is_empty())
                    .cloned()
            } else {
                None
            };
            steps.push(ChildTranscriptStep::Report {
                message,
                run_id: report_run_id.clone(),
                text,
            });
            continue;
        }
        if message.role == Role::Assistant {
            let ordinary_is_proven = message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
                || coordinates
                    .get(&message.id.0)
                    .is_some_and(|(run_id, step, _)| {
                        ordinary_response_steps.contains(&(run_id.clone(), *step))
                    });
            if !ordinary_is_proven {
                continue;
            }
        }
        steps.push(ChildTranscriptStep::Fold(message));
    }
    Ok(ChildTranscriptPlan::Ordinary { steps, coordinates })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    fn completed(run: &RunId) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(9, 0).unwrap(),
            source_commit_cursor: 9,
            thread_id: ThreadId("child".into()),
            run_id: run.clone(),
            kind: RunLifecycleEventKind::Completed,
            state: RunState::Ended(EndCause::NaturalEnd),
            await_reason: None,
        }
    }

    #[test]
    fn child_transcript_plan_decision_table() {
        // Causes: C1 ordinary child or Advisor; C2 the Run's state; C3 a
        // text-only response before or after its Completed boundary; C4 a tool
        // response. Effects: E1 a completed Run's report is one Report step;
        // E2 an unproven text response waits; E3 a tool response folds; E4 an
        // Advisor waits while running, consumes silently after a failure and
        // reports its latest Run after success.
        let run = RunId("run".into());
        let tool = Message::new(
            MessageId::assistant(&run, 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "call",
                "read",
                serde_json::json!({}),
            )],
        );
        let answer = Message::text(MessageId::assistant(&run, 1), Role::Assistant, "done");
        let messages = vec![tool.clone(), answer.clone()];
        let plan = |advisor, lifecycle: &[RunLifecycleEvent], state: Option<&RunState>| {
            let input = ChildTranscript {
                thread_id: "child",
                advisor,
                messages: &messages,
                lifecycle,
                latest_run_id: Some(&run),
                latest_run_state: state,
            };
            match plan_child_transcript(&input, |_| false).unwrap() {
                ChildTranscriptPlan::Ordinary { steps, .. } => steps
                    .iter()
                    .map(|step| match step {
                        ChildTranscriptStep::Report { text, .. } => {
                            format!("report:{}", text.clone().unwrap_or_default())
                        }
                        ChildTranscriptStep::Fold(message) => format!("fold:{}", message.id.0),
                    })
                    .collect::<Vec<_>>(),
                ChildTranscriptPlan::Advisor { consume, report } => vec![format!(
                    "advisor:consume={}:report={}",
                    consume.len(),
                    report.is_some()
                )],
            }
        };
        assert_eq!(
            plan(false, &[], Some(&RunState::Running)),
            vec![format!("fold:{}", tool.id.0)],
            "E2+E3 the text response waits for its boundary"
        );
        assert_eq!(
            plan(
                false,
                &[completed(&run)],
                Some(&RunState::Ended(EndCause::NaturalEnd))
            ),
            vec![format!("fold:{}", tool.id.0), "report:done".to_string()],
            "E1 the completed Run's report"
        );
        assert_eq!(
            plan(true, &[], Some(&RunState::Running)),
            vec!["advisor:consume=0:report=false".to_string()],
            "E4 a running consultation waits"
        );
        assert_eq!(
            plan(true, &[], Some(&RunState::Ended(EndCause::Cancelled))),
            vec!["advisor:consume=2:report=false".to_string()],
            "E4 a cancelled consultation consumes silently"
        );
        assert_eq!(
            plan(true, &[], Some(&RunState::Ended(EndCause::NaturalEnd))),
            vec!["advisor:consume=2:report=true".to_string()],
            "E4 a completed consultation reports"
        );
    }
}
