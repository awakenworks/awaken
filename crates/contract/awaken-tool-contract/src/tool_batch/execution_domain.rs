// @formal surface requirement=runtime.run_tool_lifecycle.lifecycle
// @formal reason: Pre-entry domains and immutable history extend the one ToolBatch aggregate; adjacent codec/history tables cover parsing and scope drift, without a second receipt store or direct proof of external containment.
//! Pre-entry effect domains and their immutable canonical history.
//! This extends ToolBatch; it owns no persistence or execution path.

use super::*;

/// Immutable resource provenance label supplied by trusted composition. Runtime
/// neither interprets the key nor grants resource ownership or stop authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ResourceOriginRef(String);

impl ResourceOriginRef {
    pub fn new(key: impl Into<String>) -> Result<Self, ToolBatchError> {
        let key = key.into();
        if key.trim().is_empty() {
            return Err(ToolBatchError::InvalidPersistedState(
                "resource origin requires its original nonempty key".into(),
            ));
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ResourceOriginRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Effect containment published by the concrete implementation, never by the
/// model or an authored recovery policy. A process reference is not a receipt;
/// its independent provider must observe exact closure before release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolExecutionDomain {
    Opaque {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_origin: Option<ResourceOriginRef>,
    },
    Worker {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_origin: Option<ResourceOriginRef>,
    },
    ControlledProcess {
        provider_ref: String,
        execution_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_origin: Option<ResourceOriginRef>,
    },
}

impl Default for ToolExecutionDomain {
    fn default() -> Self {
        Self::Opaque {
            resource_origin: None,
        }
    }
}

impl ToolExecutionDomain {
    /// Local execution containment, independent of borrowed resources.
    pub fn worker() -> Self {
        Self::Worker {
            resource_origin: None,
        }
    }

    /// Origin is a resource dependency, orthogonal to execution containment.
    pub fn resource_origin(&self) -> Option<&ResourceOriginRef> {
        match self {
            Self::Opaque { resource_origin }
            | Self::Worker { resource_origin }
            | Self::ControlledProcess {
                resource_origin, ..
            } => resource_origin.as_ref(),
        }
    }

    pub fn bind_resource_origin(
        mut self,
        origin: Option<&ResourceOriginRef>,
    ) -> Result<Self, ToolBatchError> {
        if self
            .resource_origin()
            .zip(origin)
            .is_some_and(|(old, next)| old != next)
        {
            return Err(ToolBatchError::InvalidPersistedState(
                "executor and borrowed resource origins differ".into(),
            ));
        }
        match &mut self {
            Self::Opaque { resource_origin }
            | Self::Worker { resource_origin }
            | Self::ControlledProcess {
                resource_origin, ..
            } => {
                if resource_origin.is_none() {
                    *resource_origin = origin.cloned();
                }
            }
        }
        Ok(self)
    }

    pub fn is_worker_contained(&self) -> bool {
        matches!(self, Self::Worker { .. })
    }

    pub fn controlled_process_binding(&self) -> Option<(&str, &str)> {
        match self {
            Self::ControlledProcess {
                provider_ref,
                execution_ref,
                ..
            } => Some((provider_ref, execution_ref)),
            _ => None,
        }
    }

    /// Compare only containment; complete Eq still freezes origin in history.
    /// Neither comparison certifies that borrowed resources are closed.
    pub fn same_containment_as(&self, other: &Self) -> bool {
        (self.is_worker_contained() && other.is_worker_contained())
            || match (
                self.controlled_process_binding(),
                other.controlled_process_binding(),
            ) {
                (Some(left), Some(right)) => left == right,
                (None, None) => matches!((self, other), (Self::Opaque { .. }, Self::Opaque { .. })),
                _ => false,
            }
    }

    pub fn validate(&self) -> Result<(), ToolBatchError> {
        if let Self::ControlledProcess {
            provider_ref,
            execution_ref,
            ..
        } = self
            && (provider_ref.trim().is_empty() || execution_ref.trim().is_empty())
        {
            return Err(ToolBatchError::InvalidPersistedState(
                "process containment requires an exact provider binding".into(),
            ));
        }
        Ok(())
    }
}

impl ToolBatch {
    pub fn mark_executing(&mut self, call_id: &str) -> Result<u16, ToolBatchError> {
        self.mark_executing_in_domain(call_id, None)
    }

    pub fn bind_materialization_domain(
        &mut self,
        call_id: &str,
        domain: ToolExecutionDomain,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        domain.validate()?;
        let call = self.call_mut(call_id)?;
        if call
            .materialization_domain
            .as_ref()
            .is_some_and(|old| old != &domain)
        {
            return Err(ToolBatchError::InvalidPersistedState(
                "result materialization changed its containment".into(),
            ));
        }
        call.materialization_domain = Some(domain);
        Ok(())
    }

    pub fn mark_executing_in_domain(
        &mut self,
        call_id: &str,
        domain: Option<ToolExecutionDomain>,
    ) -> Result<u16, ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        let phase = phase_view(&call.phase);
        let attempt = next_execution_attempt(
            phase,
            call.recovery_policy.max_attempts(),
            ExecutionRequest::StartOrRetry,
        )?;
        install_execution_entry(call, attempt, domain)?;
        Ok(attempt)
    }
}

/// The same data/phase write for fresh, recovered and approved entries. The
/// original transition kernels decide the attempt before this sole installer;
/// intrinsic validation precedes every mutation, including materialization reset.
pub(super) fn install_execution_entry(
    call: &mut DurableToolCall,
    attempt: u16,
    domain: Option<ToolExecutionDomain>,
) -> Result<(), ToolBatchError> {
    if let Some(domain) = &domain {
        domain.validate()?;
    }
    call.phase = ToolCallPhase::Executing { attempt };
    call.execution_domain = domain;
    call.materialization_domain = None;
    Ok(())
}

/// A staged transition into a genuinely new entry, before domain preparation.
/// It is not the durable execution admission proof: dropping it changes nothing,
/// and only consuming it installs data for the caller's original ThreadCommit.
#[derive(Debug)]
pub struct PendingToolExecutionEntry<'a> {
    call: &'a mut DurableToolCall,
    attempt: u16,
}

impl<'a> PendingToolExecutionEntry<'a> {
    pub(super) fn new(call: &'a mut DurableToolCall, attempt: u16) -> Self {
        Self { call, attempt }
    }

    pub fn enter(self, domain: Option<ToolExecutionDomain>) -> Result<u16, ToolBatchError> {
        install_execution_entry(self.call, self.attempt, domain)?;
        Ok(self.attempt)
    }
}

/// Retained writes use the same phase/attempt laws as the aggregate. Position
/// only addresses a transition actually present in the original prefix.
fn history_entry_anchor(
    previous: Option<(CallPhaseView, usize)>,
    phase: CallPhaseView,
    position: usize,
    max_attempts: std::num::NonZeroU16,
) -> Result<usize, ToolBatchError> {
    let invalid = || {
        ToolBatchError::InvalidPersistedState(
            "tool history violates its original entry transition".into(),
        )
    };
    let Some((old, anchor)) = previous else {
        return Ok(position);
    };
    if old == phase {
        return Ok(anchor);
    }
    if let CallPhaseView::Executing(attempt) = phase {
        // State retains the phase/attempt only. It cannot re-prove a
        // consumed correlation. Exact kind/correlation stays exclusively at
        // resume_executing; this read projection mints no admission/receipt.
        let expected = if matches!(old, CallPhaseView::Awaiting(_)) {
            FIRST_EXECUTION_ATTEMPT
        } else {
            next_execution_attempt(old, max_attempts, ExecutionRequest::StartOrRetry)
                .map_err(|_| invalid())?
        };
        return if expected == attempt {
            Ok(position)
        } else {
            Err(invalid())
        };
    }
    let allowed = match phase {
        CallPhaseView::Requested => false,
        CallPhaseView::Awaiting(_) => permits_await_transition(old, false),
        CallPhaseView::Returned => permits_call_transition(old, CallTransition::RecordReturn),
        CallPhaseView::KnownAborted => matches!(old, CallPhaseView::Returned),
        CallPhaseView::Completed => {
            permits_call_transition(old, CallTransition::CompleteImmediate)
                || permits_call_transition(old, CallTransition::CompleteFromExecutionOrWait)
                || permits_call_transition(old, CallTransition::MaterializeReturn)
        }
        CallPhaseView::Indeterminate => {
            permits_call_transition(old, CallTransition::MarkIndeterminate)
        }
        CallPhaseView::Executing(_) => unreachable!("handled above"),
    };
    if allowed { Ok(anchor) } else { Err(invalid()) }
}

/// Read-only projections of the same validated invocation history. Neither
/// view grants execution, process termination, Resource release or cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionDomainFootprint {
    pub recorded: Vec<ToolExecutionDomain>,
    pub outstanding: Vec<ToolExecutionDomain>,
}

/// One entered invocation: batch id, call id and the log position of its entry.
type EnteredCallKey = (String, String, usize);
/// The entered call, its recovery policy and the output it completed with.
type EnteredCall = (ToolCall, ToolRecoveryPolicy, Option<ToolOutput>);

impl ActiveToolBatch {
    /// Transient history fold over the canonical cell. Each (batch, call,
    /// attempt) keeps its original containment even when a later batch replaces
    /// it. A later Set cannot hide malformed records or upgrade legacy scope.
    pub fn recorded_execution_domains(
        state: &[StateCommand],
        run_id: &RunId,
    ) -> Result<Vec<ToolExecutionDomain>, ToolBatchError> {
        Self::execution_domain_footprint(state, run_id).map(|view| view.recorded)
    }

    /// Two projections of the same validated history: all original domains and
    /// entries without a complete canonical result publication. A returned
    /// invocation is not proof that its external process or Resource stopped.
    pub fn execution_domain_footprint(
        state: &[StateCommand],
        run_id: &RunId,
    ) -> Result<ToolExecutionDomainFootprint, ToolBatchError> {
        let mut domains = std::collections::BTreeMap::new();
        let mut returned = std::collections::BTreeSet::new();
        let mut entered_calls: std::collections::BTreeMap<EnteredCallKey, EnteredCall> =
            std::collections::BTreeMap::new();
        // The original log position identifies an entry, not a fabricated epoch.
        // Resume legally returns to attempt 1, so attempt alone cannot key it.
        let mut entries = std::collections::BTreeMap::new();
        for recorded in Self::recorded_batches(state, run_id) {
            let (position, batch) = recorded?;
            let batch = Self::require_retained_batch(batch)?;
            for call in batch.calls() {
                let key = (batch.id().as_str().to_owned(), call.call.call_id.clone());
                let phase = phase_view(&call.phase);
                let previous = entries.get(&key).copied();
                let anchor = history_entry_anchor(
                    previous,
                    phase,
                    position,
                    call.recovery_policy.max_attempts(),
                )?;
                let entry_key = (key.0.clone(), key.1.clone(), anchor);
                let raw = match &call.phase {
                    ToolCallPhase::Returned(output)
                    | ToolCallPhase::KnownAborted { output, .. } => Some(output),
                    _ => None,
                };
                if let Some((original, policy, known)) = entered_calls.get(&entry_key)
                    && (original != &call.call
                        || policy != &call.recovery_policy
                        || known.as_ref().zip(raw).is_some_and(|(old, raw)| old != raw)
                        || known.as_ref().is_some_and(|old| {
                            matches!(&call.phase,
                            ToolCallPhase::Completed(output) if old.state != output.state
                                || old.is_error != output.is_error)
                        }))
                {
                    return Err(ToolBatchError::InvalidPersistedState(
                        "entered tool changed its original call or recovery policy".into(),
                    ));
                }
                if matches!(phase, CallPhaseView::Executing(_))
                    || call.materialization_domain.is_some()
                    || raw.is_some()
                {
                    let (_, _, known) = entered_calls
                        .entry(entry_key)
                        .or_insert_with(|| (call.call.clone(), call.recovery_policy.clone(), None));
                    if known.is_none() {
                        *known = raw.cloned();
                    }
                }
                if previous.is_none() && phase != CallPhaseView::Requested {
                    // An isolated tail cannot establish all earlier entries.
                    // Preserve missing-prefix uncertainty; never fill it from
                    // a later origin or manufacture an earlier position.
                    domains.insert(
                        (key.0.clone(), key.1.clone(), None, false),
                        ToolExecutionDomain::default(),
                    );
                }
                if matches!(phase, CallPhaseView::Executing(_)) {
                    let domain = call.execution_domain.clone().unwrap_or_default();
                    domain.validate()?;
                    let domain_key = (key.0.clone(), key.1.clone(), Some(anchor), false);
                    if domains.get(&domain_key).is_some_and(|old| old != &domain) {
                        return Err(ToolBatchError::InvalidPersistedState(
                            "tool entry changed its containment or resource origin".into(),
                        ));
                    }
                    domains.insert(domain_key, domain);
                } else {
                    let domain_key = (key.0.clone(), key.1.clone(), Some(anchor), false);
                    if domains.get(&domain_key).is_some_and(|old| {
                        old != &call.execution_domain.clone().unwrap_or_default()
                    }) {
                        return Err(ToolBatchError::InvalidPersistedState(
                            "returned tool changed its entered containment or resource origin"
                                .into(),
                        ));
                    }
                }
                if let Some(domain) = &call.materialization_domain {
                    domain.validate()?;
                    let domain_key = (key.0.clone(), key.1.clone(), Some(anchor), true);
                    if domains.get(&domain_key).is_some_and(|old| old != domain) {
                        return Err(ToolBatchError::InvalidPersistedState(
                            "materialization history changed its containment or resource origin"
                                .into(),
                        ));
                    }
                    domains.insert(domain_key, domain.clone());
                }
                if batch.phase == ToolBatchPhase::Finalized
                    && let ToolCallPhase::Completed(output) = &call.phase
                    && let Ok(expected) = batch.result_message_id(&call.call.call_id)
                {
                    let published = call
                        .result_messages
                        .iter()
                        .find(|message| {
                            message.role != awaken_agent_contract::agent::message::Role::System
                        })
                        .is_some_and(|message| {
                            use awaken_agent_contract::agent::{
                                content::ContentBlock, message::Role,
                            };
                            message.id == expected
                                && message.role == Role::Tool
                                && matches!(message.content.as_slice(), [ContentBlock::ToolResult {
                                    tool_use_id, content, is_error, ..
                                }] if tool_use_id == &call.call.call_id
                                    && content == &output.content && *is_error == output.is_error)
                        });
                    if published {
                        // Only this exact entry is returned. Earlier retries,
                        // waits and missing-prefix sentinels remain retained.
                        for materialization in [false, true] {
                            returned.insert((
                                key.0.clone(),
                                key.1.clone(),
                                Some(anchor),
                                materialization,
                            ));
                        }
                    }
                }
                entries.insert(key, (phase, anchor));
            }
        }
        let mut recorded = Vec::with_capacity(domains.len());
        let mut outstanding = Vec::new();
        for (key, domain) in domains {
            if !returned.contains(&key) {
                outstanding.push(domain.clone());
            }
            recorded.push(domain);
        }
        Ok(ToolExecutionDomainFootprint {
            recorded,
            outstanding,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::batch;

    #[test]
    fn published_returns_remove_only_the_exact_invocation_from_current_occupancy() {
        use awaken_agent_contract::agent::{
            content::ContentBlock,
            message::{Message, Role},
        };
        // Cause graph: original Requested prefix P, fixed entered call/domain D,
        // Completed result C, Finalized publication F and exact prepared message M.
        // R1=P+D+C+F+M => retain provenance, remove current invocation occupancy.
        // R2=!C|!F|!M => retain both execution and materialization. Indeterminate
        // is not C. R3=!P => retain the opaque missing-prefix sentinel even when
        // the later returned entry is complete. No history or input is changed.
        let run = RunId("r".into());
        let bind = |batch: &ToolBatch| {
            let mut command = ActiveToolBatch::write(&Some(batch.clone()));
            command.run_id = Some(run.clone());
            command
        };
        for row in [
            "published",
            "executing",
            "indeterminate",
            "raw-completed",
            "unfinalized",
            "foreign-message",
            "foreign-content",
        ] {
            let mut candidate = batch();
            let requested = bind(&candidate);
            candidate
                .mark_executing_in_domain("c", Some(ToolExecutionDomain::default()))
                .unwrap();
            candidate
                .bind_materialization_domain("c", ToolExecutionDomain::worker())
                .unwrap();
            let entered = bind(&candidate);
            if row == "indeterminate" {
                candidate
                    .mark_indeterminate("c", "remote outcome unresolved")
                    .unwrap();
                candidate.finalize().unwrap();
            } else if row != "executing" {
                let output = ToolOutput::ok("c", "done");
                candidate.complete(output.clone()).unwrap();
                if row != "raw-completed" {
                    let id = if row == "foreign-message" {
                        awaken_agent_contract::agent::message::Id::tool_result("foreign")
                    } else {
                        candidate.result_message_id("c").unwrap()
                    };
                    candidate
                        .set_result_messages(
                            "c",
                            vec![Message::new(
                                id,
                                Role::Tool,
                                vec![ContentBlock::tool_result_with_error(
                                    "c".to_string(),
                                    if row == "foreign-content" {
                                        vec![ContentBlock::text("another return")]
                                    } else {
                                        output.content
                                    },
                                    output.is_error,
                                )],
                            )],
                        )
                        .unwrap();
                }
                if row != "unfinalized" {
                    candidate.finalize().unwrap();
                }
            }
            let state = vec![requested, entered, bind(&candidate)];
            let before = state.clone();
            let ToolExecutionDomainFootprint {
                recorded: all,
                outstanding: pending,
            } = ActiveToolBatch::execution_domain_footprint(&state, &run).unwrap();
            assert_eq!(
                all,
                ActiveToolBatch::recorded_execution_domains(&state, &run).unwrap(),
                "provenance/{row}"
            );
            assert_eq!(all.len(), 2, "both original domains/{row}");
            assert_eq!(pending.is_empty(), row == "published", "R1/R2/{row}");
            let missing = ActiveToolBatch::execution_domain_footprint(&state[1..], &run)
                .unwrap()
                .outstanding;
            assert!(
                missing
                    .iter()
                    .any(|d| matches!(d, ToolExecutionDomain::Opaque { .. })),
                "R3/{row}"
            );
            assert_eq!(state, before, "read-only/{row}");
        }
    }

    #[test]
    fn only_a_leading_system_prefix_may_precede_the_exact_first_tool_result() {
        use awaken_agent_contract::agent::{
            content::ContentBlock,
            message::{Id, Message, Role},
        };
        // J8: 0/1/2 leading Systems × exact first non-System / wrong Tool /
        // User / Assistant / prefix-only. Only the exact first non-System
        // releases this invocation; later matching results never rescue a wrong
        // first message. Existing domain and missing-prefix rows stay unchanged.
        let run = RunId("r".into());
        for prefix in 0..=2 {
            for wrong in [
                None,
                Some(Role::Tool),
                Some(Role::User),
                Some(Role::Assistant),
            ] {
                for has_result in [false, true] {
                    let mut candidate = batch();
                    let write = |batch: &ToolBatch| {
                        let mut command = ActiveToolBatch::write(&Some(batch.clone()));
                        command.bind_run(&run);
                        command
                    };
                    let requested = write(&candidate);
                    candidate
                        .mark_executing_in_domain("c", Some(ToolExecutionDomain::worker()))
                        .unwrap();
                    let entered = write(&candidate);
                    let output = ToolOutput::ok("c", "done");
                    candidate.complete(output.clone()).unwrap();
                    let mut messages = (0..prefix)
                        .map(|index| {
                            Message::text(Id(format!("context-{index}")), Role::System, "accepted")
                        })
                        .collect::<Vec<_>>();
                    if let Some(role) = wrong {
                        messages.push(Message::text(
                            Id("wrong-first".into()),
                            role,
                            "not a receipt",
                        ));
                    }
                    if has_result {
                        messages.push(Message::new(
                            candidate.result_message_id("c").unwrap(),
                            Role::Tool,
                            vec![ContentBlock::tool_result_with_error(
                                "c",
                                output.content,
                                false,
                            )],
                        ));
                    }
                    candidate.set_result_messages("c", messages).unwrap();
                    candidate.finalize().unwrap();
                    let footprint = ActiveToolBatch::execution_domain_footprint(
                        &[requested, entered, write(&candidate)],
                        &run,
                    )
                    .unwrap();
                    assert_eq!(
                        footprint.outstanding.is_empty(),
                        wrong.is_none() && has_result,
                        "prefix={prefix}, wrong={wrong:?}, result={has_result}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_later_return_cannot_close_an_earlier_entry_or_hide_input_drift() {
        use awaken_agent_contract::agent::{
            content::ContentBlock,
            message::{Message, Role},
        };
        // R4: entry A -> Awaiting -> a new entry B -> published return closes B
        // only; A remains unresolved. Entry anchors, not reused attempt numbers,
        // select completion. R5: changing entered arguments or recovery policy
        // is corruption in BOTH projections, even behind a later positive return.
        let run = RunId("r".into());
        let bind = |batch: &ToolBatch| {
            let mut command = ActiveToolBatch::write(&Some(batch.clone()));
            command.run_id = Some(run.clone());
            command
        };
        let mut candidate = batch();
        let requested = bind(&candidate);
        candidate
            .mark_executing_in_domain("c", Some(ToolExecutionDomain::default()))
            .unwrap();
        let first = bind(&candidate);
        let mut drift = serde_json::to_value(&candidate).unwrap();
        drift["calls"][0]["call"]["arguments"] = serde_json::json!({"another": true});
        let changed: ToolBatch = serde_json::from_value(drift).unwrap();
        let drift = [requested.clone(), first.clone(), bind(&changed)];
        assert!(
            ActiveToolBatch::execution_domain_footprint(&drift, &run).is_err(),
            "R5 input"
        );
        assert!(
            ActiveToolBatch::recorded_execution_domains(&drift, &run).is_err(),
            "one history validator"
        );
        let mut drift = serde_json::to_value(&candidate).unwrap();
        drift["calls"][0]["recovery_policy"]["max_attempts"] =
            (candidate.calls()[0].recovery_policy.max_attempts().get() + 1).into();
        let changed: ToolBatch = serde_json::from_value(drift).unwrap();
        assert!(
            ActiveToolBatch::execution_domain_footprint(
                &[requested.clone(), first.clone(), bind(&changed)],
                &run
            )
            .is_err(),
            "R5 policy"
        );
        candidate
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "approval")
            .unwrap();
        let waiting = bind(&candidate);
        candidate
            .resume_executing("c", ToolWaitKind::ToolPermission, "approval")
            .unwrap()
            .enter(Some(ToolExecutionDomain::worker()))
            .unwrap();
        let second = bind(&candidate);
        let output = ToolOutput::ok("c", "done");
        candidate.complete(output.clone()).unwrap();
        candidate
            .set_result_messages(
                "c",
                vec![Message::new(
                    candidate.result_message_id("c").unwrap(),
                    Role::Tool,
                    vec![ContentBlock::tool_result_with_error(
                        "c".to_string(),
                        output.content,
                        false,
                    )],
                )],
            )
            .unwrap();
        candidate.finalize().unwrap();
        let view = ActiveToolBatch::execution_domain_footprint(
            &[requested, first, waiting, second, bind(&candidate)],
            &run,
        )
        .unwrap();
        assert_eq!(view.recorded.len(), 2);
        assert_eq!(
            view.outstanding,
            vec![ToolExecutionDomain::default()],
            "R4 earlier entry retained"
        );
    }

    #[test]
    fn resource_origin_is_orthogonal_and_the_old_closed_codec_rejects_it() {
        // W3 kind(Worker/process/opaque) x origin(None/Some) preserves only
        // containment. None keeps exact historical JSON; Some must fail the
        // pinned old reader, not disappear through an ignored sibling field.
        // W4 blank/origin-object/extra fields reject; labels never mint proof.
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum OldDomain {
            Worker {},
            Opaque {},
            ControlledProcess {
                provider_ref: String,
                execution_ref: String,
            },
        }
        for original in [
            ToolExecutionDomain::Worker {
                resource_origin: None,
            },
            ToolExecutionDomain::Opaque {
                resource_origin: None,
            },
            ToolExecutionDomain::ControlledProcess {
                provider_ref: "p".into(),
                execution_ref: "uid".into(),
                resource_origin: None,
            },
        ] {
            let wire = serde_json::to_value(&original).unwrap();
            let old = serde_json::from_value::<OldDomain>(wire.clone()).unwrap();
            if let OldDomain::ControlledProcess {
                provider_ref,
                execution_ref,
            } = old
            {
                assert_eq!(
                    (provider_ref.as_str(), execution_ref.as_str()),
                    ("p", "uid")
                );
            }
            let a = original
                .clone()
                .bind_resource_origin(Some(&ResourceOriginRef::new("a").unwrap()))
                .unwrap();
            let b = original
                .clone()
                .bind_resource_origin(Some(&ResourceOriginRef::new("b").unwrap()))
                .unwrap();
            assert!(a.same_containment_as(&b));
            assert_eq!(a.is_worker_contained(), original.is_worker_contained());
            assert_ne!(a, b, "history keeps the complete provenance value");
            let mut new_wire = serde_json::to_value(&a).unwrap();
            assert!(serde_json::from_value::<OldDomain>(new_wire.clone()).is_err());
            assert_eq!(
                serde_json::from_value::<ToolExecutionDomain>(new_wire.clone()).unwrap(),
                a
            );
            for invalid in [serde_json::json!(" "), serde_json::json!({"key":"a"})] {
                new_wire["resource_origin"] = invalid;
                assert!(serde_json::from_value::<ToolExecutionDomain>(new_wire.clone()).is_err());
            }
            assert!(
                a.bind_resource_origin(Some(&ResourceOriginRef::new("b").unwrap()))
                    .is_err(),
                "two origins cannot silently overwrite"
            );
            assert!(wire.get("resource_origin").is_none());
        }
    }

    #[test]
    fn pending_entry_drop_or_invalid_domain_leaves_the_entire_call_unchanged() {
        // E1 canonical fresh/recovery admission only stages a borrowed entry;
        // drop (including cancelled/failed async preparation) changes nothing.
        // E2 invalid data also leaves phase/domain/spill/results untouched;
        // E3 consumption installs precisely the chosen attempt, once. This
        // uncommitted view is distinct from execution_admission after persist.
        let mut candidate = super::super::tests::batch_with_policy(ToolRecoveryPolicy::default());
        let before = candidate.clone();
        match candidate
            .recover_execution("c", crate::tool::ToolRecoveryCapability::NonRecoverable)
            .unwrap()
        {
            ToolRecoveryAction::Entered { entry } => {
                assert_eq!(entry.attempt, 1);
                // End the borrow; cancellation/preparation failure has no install.
            }
            _ => panic!("Requested NeverReplay may enter once"),
        }
        assert_eq!(candidate, before, "E1");
        let bad = ToolExecutionDomain::ControlledProcess {
            provider_ref: " ".into(),
            execution_ref: "uid".into(),
            resource_origin: None,
        };
        match candidate
            .recover_execution("c", crate::tool::ToolRecoveryCapability::NonRecoverable)
            .unwrap()
        {
            ToolRecoveryAction::Entered { entry } => assert!(entry.enter(Some(bad)).is_err()),
            _ => panic!("E2 actual pending entry"),
        }
        assert_eq!(candidate, before, "E2");
        candidate
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "approval")
            .unwrap();
        let waiting = candidate.clone();
        {
            let _pending = candidate
                .resume_executing("c", ToolWaitKind::ToolPermission, "approval")
                .unwrap();
        }
        assert_eq!(candidate, waiting, "E1 exact wait drop");
        assert!(
            candidate
                .resume_executing("c", ToolWaitKind::ToolPermission, "foreign")
                .is_err()
        );
        assert_eq!(candidate, waiting, "foreign wait is unchanged");
        assert_eq!(
            candidate
                .resume_executing("c", ToolWaitKind::ToolPermission, "approval")
                .unwrap()
                .enter(None)
                .unwrap(),
            1,
            "E3 no double increment"
        );
    }

    #[test]
    fn origin_and_spill_history_survive_same_attempt_number_reentry() {
        // H1 initial entry/spill A -> Awaiting -> exact resume(entry 1 again)
        // and spill B retain all four original facts. H2 continuous Executing
        // origin drift rejects; H3 old missing entry metadata remains opaque
        // after a genuinely new entry, rather than being backfilled from B.
        let run = RunId("r".into());
        let bind = |batch: &ToolBatch| {
            let mut command = ActiveToolBatch::write(&Some(batch.clone()));
            command.run_id = Some(run.clone());
            command
        };
        let domain = |key| ToolExecutionDomain::Worker {
            resource_origin: Some(ResourceOriginRef::new(key).unwrap()),
        };
        let mut candidate = batch();
        candidate
            .mark_executing_in_domain("c", Some(domain("a")))
            .unwrap();
        candidate
            .bind_materialization_domain("c", domain("spill-a"))
            .unwrap();
        let entered = bind(&candidate);
        let mut drift = serde_json::to_value(&candidate).unwrap();
        drift["calls"][0]["execution_domain"]["resource_origin"] = "b".into();
        let drift: ToolBatch = serde_json::from_value(drift).unwrap();
        assert!(
            ActiveToolBatch::recorded_execution_domains(&[entered.clone(), bind(&drift)], &run)
                .is_err(),
            "H2"
        );
        candidate
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "approval")
            .unwrap();
        let waiting = bind(&candidate);
        candidate
            .resume_executing("c", ToolWaitKind::ToolPermission, "approval")
            .unwrap()
            .enter(Some(domain("b")))
            .unwrap();
        assert!(
            candidate.calls()[0].materialization_domain.is_none(),
            "only the new current slot resets"
        );
        candidate
            .bind_materialization_domain("c", domain("spill-b"))
            .unwrap();
        let resumed = bind(&candidate);
        let retained =
            ActiveToolBatch::recorded_execution_domains(&[entered, waiting, resumed], &run)
                .unwrap();
        for key in ["a", "b", "spill-a", "spill-b"] {
            assert!(retained.contains(&domain(key)), "H1 {key}");
        }
        let mut legacy = batch();
        legacy.mark_executing("c").unwrap();
        let old = bind(&legacy);
        legacy
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "legacy")
            .unwrap();
        let wait = bind(&legacy);
        legacy
            .resume_executing("c", ToolWaitKind::ToolPermission, "legacy")
            .unwrap()
            .enter(Some(domain("b")))
            .unwrap();
        let retained =
            ActiveToolBatch::recorded_execution_domains(&[old, wait, bind(&legacy)], &run).unwrap();
        assert!(
            retained.contains(&ToolExecutionDomain::default()),
            "H3 original missing remains opaque"
        );
        assert!(retained.contains(&domain("b")), "H3 only new real entry");
    }

    #[test]
    fn history_rejects_skipped_and_rewound_entries_without_promoting_partial_prefix() {
        // H4 true retry n+1 / exact Awaiting->1 are the original kernels' numeric
        // boundaries. H5 skipped attempt, rollback to Requested and terminal
        // reentry reject. H6 a standalone historical tail adds opaque Unknown;
        // later Some does not fill missing first effects. State-only history
        // never asserts consumed correlation authenticity; producer tests own it.
        let run = RunId("r".into());
        let bind = |batch: &ToolBatch| {
            let mut command = ActiveToolBatch::write(&Some(batch.clone()));
            command.run_id = Some(run.clone());
            command
        };
        let domain = ToolExecutionDomain::worker()
            .bind_resource_origin(Some(&ResourceOriginRef::new("original").unwrap()))
            .unwrap();
        let mut candidate = ToolBatch::for_step(
            run.clone(),
            0,
            [(
                ToolCall {
                    call_id: "c".into(),
                    tool_id: "t".into(),
                    arguments: serde_json::json!({}),
                },
                crate::tool::ToolRecoveryPolicy::try_new(
                    crate::tool::ToolRecoveryMode::ReplaySafe,
                    3,
                )
                .unwrap(),
            )],
        )
        .unwrap();
        let requested = bind(&candidate);
        candidate
            .mark_executing_in_domain("c", Some(domain.clone()))
            .unwrap();
        let first = bind(&candidate);
        for phase in [
            serde_json::json!({"Executing":{"attempt":3}}),
            serde_json::json!("Requested"),
        ] {
            let mut bad = serde_json::to_value(&candidate).unwrap();
            bad["calls"][0]["phase"] = phase;
            let bad: ToolBatch = serde_json::from_value(bad).unwrap();
            assert!(
                ActiveToolBatch::recorded_execution_domains(
                    &[requested.clone(), first.clone(), bind(&bad)],
                    &run
                )
                .is_err(),
                "H5"
            );
        }
        let mut waiting = candidate.clone();
        waiting
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "exact")
            .unwrap();
        let mut wrong_attempt = serde_json::to_value(&candidate).unwrap();
        wrong_attempt["calls"][0]["phase"] = serde_json::json!({"Executing":{"attempt":2}});
        let wrong_attempt: ToolBatch = serde_json::from_value(wrong_attempt).unwrap();
        assert!(
            ActiveToolBatch::recorded_execution_domains(
                &[
                    requested.clone(),
                    first.clone(),
                    bind(&waiting),
                    bind(&wrong_attempt)
                ],
                &run,
            )
            .is_err(),
            "H5 Awaiting may return only to the original first-attempt boundary",
        );
        candidate
            .mark_executing_in_domain("c", Some(domain.clone()))
            .unwrap();
        let retried = bind(&candidate);
        assert!(
            ActiveToolBatch::recorded_execution_domains(
                &[requested.clone(), first.clone(), retried],
                &run
            )
            .is_ok(),
            "H4 exact n+1"
        );
        let mut terminal = serde_json::to_value(&candidate).unwrap();
        terminal["calls"][0]["phase"] = serde_json::json!({"Indeterminate":{"reason":"closed"}});
        let terminal: ToolBatch = serde_json::from_value(terminal).unwrap();
        assert!(
            ActiveToolBatch::recorded_execution_domains(
                &[requested, first, bind(&terminal), bind(&candidate)],
                &run
            )
            .is_err(),
            "H5 terminal reentry"
        );
        let retained =
            ActiveToolBatch::recorded_execution_domains(&[bind(&candidate)], &run).unwrap();
        assert!(
            retained.contains(&ToolExecutionDomain::default()),
            "H6 unknown earlier prefix"
        );
        assert!(retained.contains(&domain), "H6 observed data remains data");
    }

    #[test]
    fn every_domain_rejects_unknown_fields_without_changing_legacy_wire_shape() {
        // W1 exact Worker/opaque/process encoding => roundtrip; W2 add a
        // sibling field to ANY variant => reject. Empty struct variants keep
        // deny_unknown_fields effective for local/opaque domains too, so a
        // discarded process field cannot silently assert Worker containment.
        for domain in [
            ToolExecutionDomain::Opaque {
                resource_origin: None,
            },
            ToolExecutionDomain::Worker {
                resource_origin: None,
            },
            ToolExecutionDomain::ControlledProcess {
                provider_ref: "p".into(),
                execution_ref: "uid".into(),
                resource_origin: None,
            },
        ] {
            let mut value = serde_json::to_value(&domain).unwrap();
            assert_eq!(
                serde_json::from_value::<ToolExecutionDomain>(value.clone()).unwrap(),
                domain
            );
            value["unrecognized_participant"] = serde_json::json!("not silently discarded");
            assert!(serde_json::from_value::<ToolExecutionDomain>(value).is_err());
        }
        assert_eq!(
            serde_json::to_value(ToolExecutionDomain::Worker {
                resource_origin: None
            })
            .unwrap(),
            serde_json::json!({"kind":"worker"})
        );
        assert_eq!(
            ToolExecutionDomain::default(),
            ToolExecutionDomain::Opaque {
                resource_origin: None
            }
        );
    }
    use super::*;

    #[test]
    fn execution_and_materialization_domains_survive_batch_replacement_without_scope_upgrade() {
        // C1 entered tool in Worker/process/legacy-opaque scope; C2 spill owns
        // another process; C3 later batch replaces the active cell; C4 same
        // entry's scope changes on return. E1 history retains both domains;
        // E2 C3 cannot erase them; E3 C4 rejects; legacy absence stays opaque.
        // Input cells are production aggregates/decoders, not a receipt fixture.
        let run = RunId("r".into());
        let bind = |value: &ToolBatch| {
            let mut command = ActiveToolBatch::write(&Some(value.clone()));
            command.run_id = Some(run.clone());
            command
        };
        for scope in [
            ToolExecutionDomain::Worker {
                resource_origin: None,
            },
            ToolExecutionDomain::Opaque {
                resource_origin: None,
            },
        ] {
            let mut first = batch();
            first
                .mark_executing_in_domain("c", Some(scope.clone()))
                .unwrap();
            let entered = bind(&first);
            let spill = ToolExecutionDomain::ControlledProcess {
                provider_ref: "provider".into(),
                execution_ref: "spill-process".into(),
                resource_origin: None,
            };
            first
                .bind_materialization_domain("c", spill.clone())
                .unwrap();
            let materializing = bind(&first);
            first.complete(ToolOutput::ok("c", "done")).unwrap();
            first.finalize().unwrap();
            let returned = bind(&first);
            let next = ToolBatch::for_step(
                run.clone(),
                1,
                [(
                    ToolCall {
                        call_id: "next".into(),
                        tool_id: "t".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolRecoveryPolicy::default(),
                )],
            )
            .unwrap();
            let records = [
                entered.clone(),
                materializing,
                returned.clone(),
                bind(&next),
            ];
            let retained = ActiveToolBatch::recorded_execution_domains(&records, &run).unwrap();
            assert!(retained.contains(&scope));
            assert!(retained.contains(&spill));
            let mut changed = serde_json::to_value(first).unwrap();
            changed["calls"][0]["execution_domain"] = serde_json::to_value(
                if scope
                    == (ToolExecutionDomain::Worker {
                        resource_origin: None,
                    })
                {
                    ToolExecutionDomain::Opaque {
                        resource_origin: None,
                    }
                } else {
                    ToolExecutionDomain::Worker {
                        resource_origin: None,
                    }
                },
            )
            .unwrap();
            let changed: ToolBatch = serde_json::from_value(changed).unwrap();
            assert!(
                ActiveToolBatch::recorded_execution_domains(
                    &[entered, returned, bind(&changed)],
                    &run
                )
                .is_err()
            );
        }
    }
}
