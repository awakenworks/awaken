//! Run-scoped evidence for actual model executor invocations, not usage or a
//! provider terminal receipt. The existing ThreadCommit log owns persistence.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::resolved::ModelBinding;
use crate::{MergePolicy, Scope};
use awaken_agent_contract::agent::state::StateKey;

/// Evidence about one invocation, independent of response quality and error
/// classification. Missing evidence is unknown, including legacy Returned.
/// Only the trusted adapter may establish NotDispatched or Terminated; a local
/// timeout, dropped stream, HTTP status or request id is not such a proof.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelExecutionEvidence {
    #[default]
    Unknown,
    NotDispatched,
    Terminated,
}

impl ModelExecutionEvidence {
    /// Physical permission only. Retry policy, current claim and budget remain
    /// independent mandatory gates owned by Runtime/Dispatch.
    #[must_use]
    pub const fn permits_reentry(self) -> bool {
        matches!(self, Self::NotDispatched | Self::Terminated)
    }
}

/// Local observation only. In particular, a returned transport error or a
/// dropped timeout future does not prove remote computation has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelOperationPhase {
    /// Entry intent committed before calling the executor. A crash between
    /// this commit and the call is deliberately indistinguishable from entry.
    Entered,
    Returned,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperation {
    pub ordinal: NonZeroU64,
    pub binding: ModelBinding,
    pub phase: ModelOperationPhase,
    #[serde(default)]
    pub execution: ModelExecutionEvidence,
}

impl ModelOperation {
    /// A retained partial requires positive predecessor evidence. Absence is
    /// admissible only for fresh inference, never as a historical stop proof.
    #[must_use]
    pub fn permits_reentry(previous: Option<&Self>, recovering_checkpoint: bool) -> bool {
        previous.map_or(!recovering_checkpoint, |operation| {
            operation.execution.permits_reentry()
        })
    }

    /// Sequence allocation never wraps and does not infer quiescence from any
    /// prior phase. Admission/physical exclusion still belongs to Dispatch.
    pub fn next(previous: Option<&Self>, binding: ModelBinding) -> Option<Self> {
        next_ordinal(previous.map_or(0, |value| value.ordinal.get())).map(|ordinal| Self {
            ordinal,
            binding,
            phase: ModelOperationPhase::Entered,
            execution: ModelExecutionEvidence::Unknown,
        })
    }
}

fn next_ordinal(previous: u64) -> Option<NonZeroU64> {
    previous.checked_add(1).and_then(NonZeroU64::new)
}

/// Identity derived from the existing Run-scoped model journal. This is trusted
/// execution context, never model input, a provider response id or a stop proof.
/// There is no second sequence allocator or independently persisted identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOperationContext {
    thread_id: awaken_agent_contract::agent::thread::Id,
    run_id: awaken_agent_contract::agent::run::Id,
    ordinal: NonZeroU64,
    binding: ModelBinding,
}

impl ModelOperationContext {
    /// Runtime publishes this context only after the corresponding entry write
    /// succeeds. Recovery may reconstruct the same identity from that record;
    /// constructing it alone does not authorize dispatch or replay.
    #[must_use]
    pub fn from_operation(
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
        operation: &ModelOperation,
    ) -> Self {
        Self {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            ordinal: operation.ordinal,
            binding: operation.binding.clone(),
        }
    }

    #[must_use]
    pub fn run_id(&self) -> &awaken_agent_contract::agent::run::Id {
        &self.run_id
    }

    #[must_use]
    pub fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    /// A versioned, bounded coordinate for infrastructure correlation. Hash the
    /// structured tuple so delimiters inside opaque ids cannot create aliases.
    /// Equality identifies one invocation, not permission to invoke it again.
    #[must_use]
    pub fn operation_id(&self) -> String {
        let fingerprint = crate::resolution::content_fingerprint(&(
            "model-operation-v1",
            &self.thread_id,
            &self.run_id,
            self.ordinal,
            &self.binding,
        ))
        .expect("model operation identity components always serialize");
        format!("model-op-v1:{fingerprint}")
    }
}

tokio::task_local! {
    static MODEL_OPERATION_CONTEXT: Option<ModelOperationContext>;
}

/// Read the Runtime-owned identity at provider entry. Direct calls outside an
/// instrumented invocation have no durable identity and must not fabricate one.
#[must_use]
pub fn current_model_operation_context() -> Option<ModelOperationContext> {
    MODEL_OPERATION_CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
}

/// Scope one provider invocation, including adapter awaits, to its journal
/// identity. `None` explicitly masks an outer invocation rather than borrowing
/// its key. Child tasks must receive their own explicit execution context.
pub async fn with_model_operation_context<T>(
    context: Option<ModelOperationContext>,
    future: impl std::future::Future<Output = T>,
) -> T {
    MODEL_OPERATION_CONTEXT.scope(context, future).await
}

/// One latest-value projection; older invocations remain in the append-only
/// Thread state log. ToolBatch and model usage keep their existing owners.
pub struct ActiveModelOperation;

impl StateKey for ActiveModelOperation {
    const KEY: &'static str = "runtime.model_operation.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = Option<ModelOperation>;
}

impl ActiveModelOperation {
    /// Read the exact Run's evidence at a committed lifecycle boundary. A later
    /// receipt must not rewrite an already published historical error. Reuse the
    /// typed cell and state materializer; this observer grants no recovery rights.
    pub fn at_commit(
        state: &[awaken_agent_contract::agent::state::Command],
        cursors: &[u64],
        run_id: &awaken_agent_contract::agent::run::Id,
        through: u64,
    ) -> Result<Option<ModelOperation>, awaken_agent_contract::agent::state::StateError> {
        use awaken_agent_contract::agent::state::{StateError, Store};
        let belongs = |command: &&awaken_agent_contract::agent::state::Command| {
            command.scope == Self::SCOPE
                && command.key.0 == Self::KEY
                && command.run_id.as_ref() == Some(run_id)
        };
        if !state.iter().any(|command| belongs(&command)) {
            return Ok(None);
        }
        if state.len() != cursors.len() {
            return Err(StateError {
                key: Self::KEY.into(),
                scope: Self::SCOPE,
                detail: "model evidence has no aligned commit coordinates".into(),
            });
        }
        let mut store = Store::new();
        for (command, cursor) in state.iter().zip(cursors) {
            if belongs(&command) && *cursor <= through {
                store.apply(command);
            }
        }
        Self::load(&store)
    }
}

#[cfg(kani)]
#[kani::proof]
fn model_execution_reentry_requires_positive_evidence() {
    // R1 unknown => deny; R2 not dispatched / terminal => only the physical
    // gate opens. Retry policy and claim/budget remain separate authorities.
    let selector: u8 = kani::any();
    let evidence = match selector % 3 {
        0 => ModelExecutionEvidence::Unknown,
        1 => ModelExecutionEvidence::NotDispatched,
        _ => ModelExecutionEvidence::Terminated,
    };
    assert_eq!(evidence.permits_reentry(), selector % 3 != 0);
    // R3 missing journal + retained checkpoint => deny; R4 neither => fresh
    // entry allowed. Cross R1..R4 with each local phase: phase cannot authorize.
    // This exhausts the production decision, not persistence or adapter truth.
    let present: bool = kani::any();
    let recovering_checkpoint: bool = kani::any();
    let phase_selector: u8 = kani::any();
    let operation = ModelOperation {
        ordinal: NonZeroU64::MIN,
        binding: ModelBinding::new("", "", ""),
        phase: match phase_selector % 3 {
            0 => ModelOperationPhase::Entered,
            1 => ModelOperationPhase::Returned,
            _ => ModelOperationPhase::TimedOut,
        },
        execution: evidence,
    };
    assert_eq!(
        ModelOperation::permits_reentry(present.then_some(&operation), recovering_checkpoint),
        if present {
            selector % 3 != 0
        } else {
            !recovering_checkpoint
        }
    );
}

#[cfg(kani)]
#[kani::proof]
fn model_operation_ordinal_never_wraps_or_reuses_a_coordinate() {
    // C1 previous < MAX / C2 previous == MAX; E1 exact nonzero successor /
    // E2 refusal. R1=C1=>E1; R2=C2=>E2. Exhaust the production u64 kernel.
    let previous: u64 = kani::any();
    match next_ordinal(previous) {
        Some(next) => {
            assert!(previous < u64::MAX);
            assert!(next.get() > previous);
            assert_eq!(next.get() - previous, 1);
            // C3 adapter identity is projected from this record, not allocated
            // by a second counter. Exhaust the full-width ordinal projection;
            // string tuple isolation and task-local propagation use Rust tests.
            // This does not prove the fingerprint or any provider-side effect.
            let operation = ModelOperation {
                ordinal: next,
                binding: ModelBinding::new("", "", ""),
                phase: ModelOperationPhase::Entered,
                execution: ModelExecutionEvidence::Unknown,
            };
            let identity = ModelOperationContext::from_operation(
                &awaken_agent_contract::agent::thread::Id(String::new()),
                &awaken_agent_contract::agent::run::Id(String::new()),
                &operation,
            );
            assert_eq!(identity.ordinal, next);
        }
        None => assert_eq!(previous, u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn model_identity_is_reconstructible_exact_and_task_scoped() {
        use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
        // I1 same persisted tuple reconstructed, regardless of local phase =>
        // same id. I2 change each coordinate independently => distinct id. I3
        // concurrent/nested scopes => isolated; I4 absent/direct scope => None.
        // Hash collision resistance is TCB; this checks serialization boundaries,
        // not provider idempotency, replay authorization or stop evidence.
        let thread = ThreadId("thread:a".into());
        let run = RunId("run:b".into());
        let record = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
        let context = ModelOperationContext::from_operation(&thread, &run, &record);
        let mut restored: ModelOperation =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        restored.phase = ModelOperationPhase::Returned;
        restored.execution = ModelExecutionEvidence::Terminated;
        assert_eq!(
            context,
            ModelOperationContext::from_operation(&thread, &run, &restored)
        );
        for field in 0..6 {
            let mut changed = context.clone();
            match field {
                0 => changed.thread_id.0.push('x'),
                1 => changed.run_id.0.push('x'),
                2 => changed.ordinal = NonZeroU64::new(2).unwrap(),
                3 => changed.binding.provider_identity_ref.push('x'),
                4 => changed.binding.model_ref.push('x'),
                _ => changed.binding.backend_ref.push('x'),
            }
            assert_ne!(
                context.operation_id(),
                changed.operation_id(),
                "I2 field {field}"
            );
            let observe = |value: ModelOperationContext| async move {
                with_model_operation_context(Some(value.clone()), async {
                    tokio::task::yield_now().await;
                    assert_eq!(current_model_operation_context(), Some(value.clone()));
                    with_model_operation_context(None, async {
                        tokio::task::yield_now().await;
                        assert_eq!(current_model_operation_context(), None);
                    })
                    .await;
                    assert_eq!(current_model_operation_context(), Some(value));
                })
                .await;
            };
            tokio::join!(observe(context.clone()), observe(changed));
            assert_eq!(current_model_operation_context(), None);
        }
        let a = ModelOperationContext::from_operation(
            &ThreadId("a:b".into()),
            &RunId("c".into()),
            &record,
        );
        let b = ModelOperationContext::from_operation(
            &ThreadId("a".into()),
            &RunId("b:c".into()),
            &record,
        );
        assert_ne!(
            a.operation_id(),
            b.operation_id(),
            "I2 delimiters cannot alias"
        );
    }

    #[test]
    fn committed_model_observation_is_exact_run_and_prefix_scoped() {
        // C1 Run A/B, C2 boundary before/at/after write, C3 later receipt,
        // C4 malformed value, C5 unaligned cursors. R1 A at entry=>Unknown;
        // R2 A at receipt=>Terminated; R3 B/earlier=>absent; R4 C4/C5=>error.
        // Effects: later evidence cannot mutate the historical observation;
        // neither another Run nor corrupt persistence fabricates completion.
        use awaken_agent_contract::agent::run::Id;
        let a = Id("a".into());
        let b = Id("b".into());
        let mut operation = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
        let mut entered = ActiveModelOperation::write(&Some(operation.clone()));
        entered.bind_run(&a);
        operation.execution = ModelExecutionEvidence::Terminated;
        let mut returned = ActiveModelOperation::write(&Some(operation));
        returned.bind_run(&a);
        let commands = [entered, returned];
        for through in 0..=3 {
            for run in [&a, &b] {
                let value =
                    ActiveModelOperation::at_commit(&commands, &[1, 3], run, through).unwrap();
                assert_eq!(
                    value.map(|v| v.execution),
                    if run == &b || through == 0 {
                        None
                    } else if through < 3 {
                        Some(ModelExecutionEvidence::Unknown)
                    } else {
                        Some(ModelExecutionEvidence::Terminated)
                    }
                );
            }
        }
        assert!(ActiveModelOperation::at_commit(&commands, &[1], &a, 2).is_err());
        let mut corrupt = commands[0].clone();
        corrupt.action =
            awaken_agent_contract::agent::state::Action::Set(serde_json::json!("corrupt"));
        assert!(ActiveModelOperation::at_commit(&[corrupt], &[1], &a, 2).is_err());
    }

    #[test]
    fn legacy_local_phases_never_create_termination_evidence() {
        // L1 old phase is Entered/Returned/TimedOut; L2 evidence field absent;
        // L3 explicit evidence Unknown/NotDispatched/Terminated. Effects: all
        // L1+L2 decode Unknown; L3 round-trips exactly, independent of phase.
        // A local Returned is never promoted to remote termination on upgrade.
        for phase in [
            ModelOperationPhase::Entered,
            ModelOperationPhase::Returned,
            ModelOperationPhase::TimedOut,
        ] {
            let mut operation =
                ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
            operation.phase = phase;
            let mut legacy = serde_json::to_value(&operation).unwrap();
            legacy.as_object_mut().unwrap().remove("execution");
            let decoded: ModelOperation = serde_json::from_value(legacy).unwrap();
            assert_eq!(decoded.execution, ModelExecutionEvidence::Unknown);
            assert!(!decoded.execution.permits_reentry());
            for execution in [
                ModelExecutionEvidence::Unknown,
                ModelExecutionEvidence::NotDispatched,
                ModelExecutionEvidence::Terminated,
            ] {
                operation.execution = execution;
                assert_eq!(
                    serde_json::from_value::<ModelOperation>(
                        serde_json::to_value(&operation).unwrap()
                    )
                    .unwrap(),
                    operation
                );
            }
        }
    }

    #[test]
    fn execution_evidence_does_not_change_failure_taxonomy_or_retry_hints() {
        use crate::llm::Error;
        use crate::resilience::Classify;
        // C1 every closed error class; C2 absent or each evidence value.
        // E1 absent => Unknown; E2 code, message, health, routing and delay
        // are preserved for all C1*C2; E3 only evidence controls physical
        // re-entry. Wrapping an already annotated error remains flat.
        for execution in [
            ModelExecutionEvidence::Unknown,
            ModelExecutionEvidence::NotDispatched,
            ModelExecutionEvidence::Terminated,
        ] {
            for error in [
                Error::Binding("binding".into()),
                Error::Provider("provider".into()),
                Error::Timeout("timeout".into()),
                Error::ContextOverflow("context".into()),
                Error::InvalidRequest("request".into()),
                Error::Unauthorized("auth".into()),
                Error::LoginRequired("login".into()),
                Error::ModelNotFound("model".into()),
                Error::ContentFiltered("filter".into()),
                Error::RateLimited {
                    message: "rate".into(),
                    retry_after: Some(std::time::Duration::from_secs(2)),
                },
                Error::Overloaded {
                    message: "overload".into(),
                    retry_after: Some(std::time::Duration::from_secs(3)),
                },
                Error::UsageLimit {
                    message: "usage".into(),
                    reset_after: Some(std::time::Duration::from_secs(4)),
                },
            ] {
                assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
                let expected = (
                    error.code(),
                    error.to_string(),
                    error.is_retryable(),
                    error.retry_after(),
                    error.reset_hint(),
                    error.disposition(),
                );
                let observed = error
                    .with_execution_evidence(execution)
                    .with_execution_evidence(execution);
                assert_eq!(
                    (
                        observed.code(),
                        observed.to_string(),
                        observed.is_retryable(),
                        observed.retry_after(),
                        observed.reset_hint(),
                        observed.disposition()
                    ),
                    expected
                );
                assert_eq!(observed.execution_evidence(), execution);
                let Error::Observed { source, .. } = observed else {
                    panic!("explicit evidence required")
                };
                assert!(!matches!(*source, Error::Observed { .. }));
            }
        }
    }

    #[test]
    fn model_operation_coordinate_and_wire_decision_table() {
        // Causes: C1 absent/ordinary/MAX predecessor; C2 any local phase;
        // C3 zero ordinal on wire. Effects: E1 exact successor; E2 MAX rejects;
        // E3 malformed zero rejects. R1=!MAX+C2=>E1; R2=MAX+C2=>E2;
        // R3=C3=>E3. No phase, including Returned, is termination authority.
        let binding = ModelBinding::new("provider", "model", "adapter");
        for previous in [0, 1, u64::MAX - 1, u64::MAX] {
            for phase in [
                ModelOperationPhase::Entered,
                ModelOperationPhase::Returned,
                ModelOperationPhase::TimedOut,
            ] {
                let prior = NonZeroU64::new(previous).map(|ordinal| ModelOperation {
                    ordinal,
                    binding: binding.clone(),
                    phase,
                    execution: ModelExecutionEvidence::Unknown,
                });
                let next = ModelOperation::next(prior.as_ref(), binding.clone());
                assert_eq!(
                    next.as_ref().map(|v| v.ordinal.get()),
                    previous.checked_add(1)
                );
                if let Some(next) = next {
                    assert_eq!(next.phase, ModelOperationPhase::Entered);
                    let mut wire = serde_json::to_value(&next).unwrap();
                    assert_eq!(
                        serde_json::from_value::<ModelOperation>(wire.clone()).unwrap(),
                        next
                    );
                    wire["ordinal"] = 0.into();
                    assert!(serde_json::from_value::<ModelOperation>(wire).is_err());
                }
            }
        }
    }
}
