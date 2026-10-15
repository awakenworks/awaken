// @formal surface exclude=read_model_contract
// @formal reason: Joins the existing frozen Session scope and original Dispatch history without writing or minting execution authority.
//! Read the complete frozen execution scope before presenting closure evidence.

use awaken_dispatch_contract::dispatch::PhysicalAttempt;
use awaken_dispatch_contract::{
    AttemptClosureRecord, AttemptPhysicalFact, DispatchCursor, DispatchOperation,
};
use awaken_session_execution_contract::{
    SessionExecutionClosureReference, SessionExecutionRecoveryGap as Gap,
    SessionExecutionRecoveryObservation as Observation,
};
use awaken_session_state_contract::PersistedSession;
use std::collections::{BTreeMap, BTreeSet};

const PAGE_SIZE: usize = 256;
const MAX_PAGES: usize = 256;

fn original_preparation(
    session: &PersistedSession,
) -> Option<awaken_session_execution_contract::SessionRecoveryPreparationReference> {
    match session.terminal_cleanup_work_action() {
        Ok(Some(awaken_session_state_contract::SessionTerminalCleanupAction::Dispose {
            command,
        })) => Some(
            awaken_session_execution_contract::SessionRecoveryPreparationReference {
                session_id: command.session_id.to_string(),
                effect_id: command.effect_id,
                preparation_fingerprint: command.preparation_fingerprint,
            },
        ),
        _ => None,
    }
}

fn response_only_run(
    snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    run: &awaken_agent_contract::agent::run::Id,
) -> bool {
    use awaken_agent_contract::agent::state::StateKey;
    use awaken_runtime_contract::attempt_termination::{
        ActiveNativeAttemptCoverage, NativeAttemptCoverage,
    };
    use awaken_runtime_contract::model_operation::{ActiveModelOperation, ModelEffectScope};
    use awaken_runtime_contract::tool_batch::{ActiveToolBatch, store_from_run_commands};
    let store = store_from_run_commands(snapshot.state.clone(), run);
    ActiveNativeAttemptCoverage::load(&store)
        .is_ok_and(|coverage| coverage != NativeAttemptCoverage::Unspecified)
        && snapshot.messages.iter().all(|message| {
            message.content.iter().all(|block| {
                !matches!(
                    block,
                    awaken_agent_contract::agent::content::ContentBlock::ToolUse { .. }
                )
            })
        })
        && ActiveToolBatch::recorded_batches(&snapshot.state, run)
            .next()
            .is_none()
        && ActiveModelOperation::recorded(&snapshot.state, run).is_ok_and(|models| {
            models
                .iter()
                .all(|model| model.effect_scope == ModelEffectScope::ResponseOnly)
        })
}

fn unknown(reason: Gap) -> Observation {
    Observation::NotProven { reason }
}

fn same_attempt(entry: &PhysicalAttempt, closed: &PhysicalAttempt) -> bool {
    entry.claim == closed.claim
        && entry.binding == closed.binding
        && entry.thread_id == closed.thread_id
        && entry.session_thread_id == closed.session_thread_id
}

fn verified_closure(
    entry: &PhysicalAttempt,
    facts: &[AttemptPhysicalFact],
) -> Result<AttemptClosureRecord, Gap> {
    entry
        .validate_admission()
        .map_err(|_| Gap::IdentityMismatch)?;
    let mut found = None;
    for fact in facts
        .iter()
        .filter(|fact| fact.attempt().claim == entry.claim)
    {
        fact.validate().map_err(|_| Gap::IdentityMismatch)?;
        if !same_attempt(entry, fact.attempt()) || found.is_some() {
            return Err(Gap::IdentityMismatch);
        }
        match fact {
            AttemptPhysicalFact::IndependentClosure { record } => found = Some(record.clone()),
            AttemptPhysicalFact::Acknowledgement { .. } => return Err(Gap::OrdinaryReturnOnly),
        }
    }
    found.ok_or(Gap::AttemptStillOpen)
}

async fn original_closure_records<
    S: awaken_dispatch_contract::DispatchAttemptRecordReads + ?Sized,
>(
    source: &S,
    run: &awaken_agent_contract::agent::run::Id,
) -> Result<Vec<AttemptPhysicalFact>, Gap> {
    let mut facts = Vec::new();
    let mut cursor = DispatchCursor::default();
    for _ in 0..MAX_PAGES {
        let page = source
            .attempt_closure_records_after(cursor, PAGE_SIZE, Some(run))
            .await
            .map_err(|_| Gap::HistoryUnavailable)?;
        if page.events.is_empty() {
            return Ok(facts);
        }
        if page.next_cursor <= cursor {
            return Err(Gap::IdentityMismatch);
        }
        facts.extend(page.events.into_iter().map(|event| event.record));
        cursor = page.next_cursor;
    }
    Err(Gap::ReadBudgetExceeded)
}

pub async fn read_execution_recovery<S, R>(
    source: &S,
    reads: &R,
    session: &PersistedSession,
) -> Observation
where
    S: awaken_dispatch_contract::DispatchOperationalFeed
        + awaken_dispatch_contract::DispatchAttemptRecordReads
        + ?Sized,
    R: awaken_session_execution_contract::SessionProjectionSource + ?Sized,
{
    if !session.is_terminal() {
        return unknown(Gap::NotTerminal);
    }
    let Ok(cleanup) = session.verified_terminal_cleanup() else {
        return unknown(Gap::IdentityMismatch);
    };
    if cleanup.is_not_requested() {
        return unknown(Gap::AdmissionFenceMissing);
    }
    let (Some(threads), Some(runtime_commit_cursor)) =
        (cleanup.thread_ids(), cleanup.runtime_commit_cursor())
    else {
        return unknown(Gap::ScopeNotFrozen);
    };
    if !threads.contains(session.session_id.as_str()) {
        return unknown(Gap::IdentityMismatch);
    }

    let mut entries = Vec::new();
    let mut cursor = DispatchCursor::default();
    let mut exhausted = true;
    for _ in 0..MAX_PAGES {
        let Ok(page) = source.events_after(cursor, PAGE_SIZE, None).await else {
            return unknown(Gap::HistoryUnavailable);
        };
        if page.events.is_empty() {
            exhausted = false;
            break;
        }
        if page.next_cursor <= cursor {
            return unknown(Gap::IdentityMismatch);
        }
        for event in page.events {
            if let DispatchOperation::AttemptAdmitted { attempt } = event.operation {
                let member = attempt
                    .thread_id
                    .as_ref()
                    .is_some_and(|thread| threads.contains(&thread.0));
                let parent = attempt
                    .session_thread_id
                    .as_ref()
                    .is_some_and(|thread| thread.0 == session.session_id.as_str());
                if parent && !member {
                    return unknown(Gap::IdentityMismatch);
                }
                if member
                    && attempt
                        .session_thread_id
                        .as_ref()
                        .is_some_and(|parent| parent.0 != session.session_id.as_str())
                {
                    return unknown(Gap::IdentityMismatch);
                }
                if member {
                    entries.push(attempt);
                }
            }
        }
        cursor = page.next_cursor;
    }
    if exhausted {
        return unknown(Gap::ReadBudgetExceeded);
    }
    // Empty history cannot backfill an older executor's missing enrollment.
    if entries.is_empty() {
        return unknown(Gap::InventoryIncomplete);
    }
    let mut response_only = true;
    for thread in threads {
        let snapshot = match reads
            .projection_thread_snapshot(&session.session_id, thread, None)
            .await
        {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return unknown(Gap::HistoryMissing),
            Err(_) => return unknown(Gap::HistoryUnavailable),
        };
        if snapshot.thread_id.0 != *thread
            || snapshot.runs.iter().any(|run| run.thread_id.0 != *thread)
        {
            return unknown(Gap::IdentityMismatch);
        }
        if snapshot.runs.iter().any(|run| {
            !entries.iter().any(|entry| {
                entry.claim.run_id == run.id
                    && entry
                        .thread_id
                        .as_ref()
                        .is_some_and(|entry_thread| entry_thread.0 == *thread)
            })
        }) {
            return unknown(Gap::InventoryIncomplete);
        }
        response_only &= snapshot
            .runs
            .iter()
            .all(|run| response_only_run(&snapshot, &run.id));
    }
    let mut identities = BTreeSet::new();
    let mut closure_records = Vec::new();
    let mut records_by_run = BTreeMap::new();
    for entry in &entries {
        if !identities.insert((
            entry.claim.run_id.0.clone(),
            entry.claim.owner.clone(),
            entry.claim.epoch,
        )) {
            return unknown(Gap::IdentityMismatch);
        }
        if !records_by_run.contains_key(&entry.claim.run_id.0) {
            match original_closure_records(source, &entry.claim.run_id).await {
                Ok(facts) => {
                    records_by_run.insert(entry.claim.run_id.0.clone(), facts);
                }
                Err(gap) => return unknown(gap),
            }
        }
        match verified_closure(entry, &records_by_run[&entry.claim.run_id.0]) {
            Ok(record) => {
                closure_records.push(SessionExecutionClosureReference {
                    run_id: record.attempt().claim.run_id.0.clone(),
                    evidence_ref: record.evidence_ref().to_owned(),
                });
            }
            Err(gap) => return unknown(gap),
        }
    }
    Observation::EffectsClosed {
        preparation: original_preparation(session),
        operation_footprint: response_only.then_some(
            awaken_session_execution_contract::SessionExecutionOperationFootprint::ResponseOnly,
        ),
        session_revision: session.revision.0,
        runtime_commit_cursor,
        thread_ids: threads.iter().cloned().collect(),
        closure_records,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_dispatch_contract::dispatch::AttemptAcknowledgementRecord;
    use awaken_dispatch_contract::{AttemptClosureKind, AttemptReturnCause, RunClaim};

    use awaken_agent_contract::agent::run::{EndCause, Record as RunRecord, RunState};
    use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
    use awaken_dispatch_contract::{AttemptClosureEvent, DispatchOperationalEvent};
    use awaken_session_types::{RunError, SessionId};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct RecoverySource {
        events: Vec<DispatchOperationalEvent>,
        closures: Option<Vec<AttemptClosureEvent>>,
        calls: AtomicUsize,
        unavailable: AtomicBool,
    }
    #[async_trait::async_trait]
    impl awaken_dispatch_contract::DispatchOperationalFeed for RecoverySource {
        async fn events_after(
            &self,
            cursor: DispatchCursor,
            limit: usize,
            run: Option<&RunId>,
        ) -> Result<
            awaken_dispatch_contract::DispatchPage,
            awaken_dispatch_contract::DispatchError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(awaken_dispatch_contract::DispatchError::Unavailable(
                    "fixture outage".into(),
                ));
            }
            let events = self
                .events
                .iter()
                .filter(|e| e.cursor > cursor)
                .filter(|e| run.is_none_or(|run| e.operation.run_id() == run))
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            let next_cursor = events.last().map_or(cursor, |e| e.cursor);
            Ok(awaken_dispatch_contract::DispatchPage {
                events,
                next_cursor,
            })
        }
    }
    #[async_trait::async_trait]
    impl awaken_dispatch_contract::DispatchAttemptRecordReads for RecoverySource {
        async fn attempt_closure_records_after(
            &self,
            cursor: DispatchCursor,
            limit: usize,
            run: Option<&RunId>,
        ) -> Result<
            awaken_dispatch_contract::AttemptClosurePage,
            awaken_dispatch_contract::DispatchError,
        > {
            let records = self.closures.as_ref().ok_or_else(|| {
                awaken_dispatch_contract::DispatchError::Unavailable("fixture outage".into())
            })?;
            let events = records
                .iter()
                .filter(|e| e.cursor > cursor)
                .filter(|e| run.is_none_or(|run| e.record.attempt().claim.run_id == *run))
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            let next_cursor = events.last().map_or(cursor, |e| e.cursor);
            Ok(awaken_dispatch_contract::AttemptClosurePage {
                events,
                next_cursor,
            })
        }
    }
    #[derive(Default)]
    struct RecoveryHistory(std::sync::Mutex<BTreeMap<String, RunRecoverySnapshot>>);
    impl RecoveryHistory {
        fn set_recovery_snapshot(
            &self,
            _session: &str,
            thread: &ThreadId,
            snapshot: RunRecoverySnapshot,
        ) {
            self.0.lock().unwrap().insert(thread.0.clone(), snapshot);
        }
    }
    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionProjectionSource for RecoveryHistory {
        async fn projection_thread_snapshot(
            &self,
            _session: &SessionId,
            thread: &str,
            _run: Option<&RunId>,
        ) -> Result<Option<RunRecoverySnapshot>, RunError> {
            Ok(self.0.lock().unwrap().get(thread).cloned())
        }
        async fn projection_coordinated_threads(
            &self,
            _session: &SessionId,
            _root: Option<&RunRecoverySnapshot>,
        ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
            Ok(Vec::new())
        }
        async fn projection_outcome(
            &self,
            _thread: &str,
            _outcome: &str,
        ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError>
        {
            Ok(None)
        }
    }
    fn recovery_snapshot(
        thread: &str,
        runs: Vec<RunRecord>,
        _messages: Vec<()>,
        _events: Vec<()>,
    ) -> RunRecoverySnapshot {
        let run = runs.last().unwrap().id.clone();
        RunRecoverySnapshot {
            thread_id: ThreadId(thread.into()),
            claimed_run_id: run.clone(),
            latest_run_id: Some(run),
            runs,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 4,
            store_cursor: 9,
            next_commit_ordinal: 2,
        }
    }
    fn recovery_session(id: &str) -> PersistedSession {
        use awaken_session_configuration_contract::{
            EnvironmentFingerprint, EnvironmentSnapshot, SessionBaseline, SessionBaselineInputs,
            SessionNetworkPolicy,
        };
        let environment = EnvironmentSnapshot {
            environment_id: "fixture-env".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: EnvironmentFingerprint("fixture".into()),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment,
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "fixture-agent".into(),
            agent_revision: Some(1),
            model: "fixture-model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        PersistedSession::frozen_with_budget(
            id,
            baseline,
            Default::default(),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        )
    }
    fn entry() -> PhysicalAttempt {
        PhysicalAttempt {
            claim: RunClaim {
                run_id: RunId("run".into()),
                owner: "worker".into(),
                epoch: 2,
            },
            binding: None,
            thread_id: Some(ThreadId("thread".into())),
            session_thread_id: Some(ThreadId("session".into())),
            closing: false,
        }
    }

    #[test]
    fn only_exact_independent_closure_proves_the_original_admission() {
        // Cause/effect graph: C1 exact admitted tuple; C2 positive independent
        // closure (All or Controlled); C3 record absent, ordinary, foreign or
        // duplicated. R1 C1+C2 => preserve original closure; R2 absent => open;
        // R3 ordinary => unproven effects; R4 foreign/duplicate => invalid.
        let admitted = entry();
        assert_eq!(
            verified_closure(&admitted, &[]),
            Err(Gap::AttemptStillOpen),
            "R2"
        );
        let ordinary = AttemptPhysicalFact::Acknowledgement {
            record: AttemptAcknowledgementRecord::try_new(
                admitted.clone(),
                AttemptReturnCause::StructuredReturnObserved,
            )
            .unwrap(),
        };
        assert_eq!(
            verified_closure(&admitted, &[ordinary]),
            Err(Gap::OrdinaryReturnOnly),
            "R3"
        );
        for kind in [
            AttemptClosureKind::AllExecutionStopped,
            AttemptClosureKind::ControlledExecutionClosed,
        ] {
            let mut closing = admitted.clone();
            closing.closing = true;
            let record =
                AttemptClosureRecord::try_new(closing, kind, "original-source-evidence").unwrap();
            let fact = AttemptPhysicalFact::IndependentClosure {
                record: record.clone(),
            };
            assert_eq!(
                verified_closure(&admitted, std::slice::from_ref(&fact)),
                Ok(record),
                "R1"
            );
            assert_eq!(
                verified_closure(&admitted, &[fact.clone(), fact.clone()]),
                Err(Gap::IdentityMismatch),
                "R4 duplicate"
            );
            let mut foreign = admitted.clone();
            foreign.thread_id = Some(ThreadId("foreign-thread".into()));
            assert_eq!(
                verified_closure(&foreign, &[fact]),
                Err(Gap::IdentityMismatch),
                "R4 foreign"
            );
        }
    }
    #[test]
    fn recovery_preparation_requires_the_original_complete_aggregate_receipts() {
        use awaken_session_configuration_contract::resource::SessionRealizationLease;
        use awaken_session_state_contract::{
            SessionCleanupPreparation, SessionCleanupRepositoryPreparation,
            SessionTerminalCleanupEffect,
        };
        // C1 fenced original Session; C2 frozen targets; C3 each exact Runtime
        // preparation and Repository receipt. R1 !C2 or !C3 -> no reference;
        // R2 C1+C2+C3 -> copy only the original Dispose command coordinates,
        // while cleanup remains pending. A changed/invalid receipt must not
        // be replaced by a boolean success label.
        let mut session = recovery_session("preparation-root");
        session.execution = awaken_session_state_contract::SessionExecutionState::Terminated;
        session.ensure_terminal_cleanup_fence();
        assert!(original_preparation(&session).is_none(), "R1 fenced only");
        session.freeze_terminal_cleanup_targets([], 0, 7).unwrap();
        assert!(
            original_preparation(&session).is_none(),
            "R1 no preparations"
        );
        let lease = SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "incarnation".into(),
            epoch: 1,
            expires_at_unix_ms: 100,
        };
        session.realization = Some(lease.clone());
        let command = session
            .terminal_cleanup
            .command_for(&session.session_id, session.session_id.as_str())
            .unwrap();
        let effect = SessionTerminalCleanupEffect::new(command, lease.clone());
        let prepared = SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            vec![],
        )
        .unwrap();
        let repository = SessionCleanupRepositoryPreparation::new(
            &session.session_id,
            "workspace",
            &session.resources,
        )
        .unwrap();
        session
            .record_terminal_cleanup_preparation("workspace", &lease, prepared, Some(repository))
            .unwrap();
        let expected = match session.terminal_cleanup_work_action().unwrap().unwrap() {
            awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } => {
                command
            }
            _ => panic!("R2 exact complete preparation must authorize only disposal"),
        };
        let observed = original_preparation(&session).unwrap();
        assert_eq!(
            observed.session_id,
            expected.session_id.to_string(),
            "R2 original Session"
        );
        assert_eq!(observed.effect_id, expected.effect_id, "R2 original effect");
        assert_eq!(
            observed.preparation_fingerprint, expected.preparation_fingerprint,
            "R2 original aggregate fingerprint"
        );
        assert!(
            !session.terminal_cleanup.is_completed(),
            "R2 no physical cleanup ACK"
        );
    }

    #[test]
    fn response_only_footprint_uses_original_coverage_and_every_operation() {
        use awaken_agent_contract::agent::state::{Command, StateKey};
        use awaken_runtime_contract::attempt_termination::{
            ActiveNativeAttemptCoverage, NativeAttemptCoverage,
        };
        use awaken_runtime_contract::model_operation::{
            ActiveModelOperation, ModelEffectScope, ModelOperation,
        };
        use awaken_runtime_contract::resolved::ModelBinding;
        use awaken_runtime_contract::tool::{ToolCall, ToolRecoveryPolicy};
        use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolBatch};
        // Causes C1 original coverage; C2 every model's frozen scope is
        // ResponseOnly; C3 no ToolBatch history. R1 C1+C2+C3 -> footprint;
        // R2 missing/malformed coverage, opaque/malformed model, any retained
        // tool batch (even Requested), or removed history -> unknown. Physical
        // closure is independently required by the sole whole-scope caller.
        let run = RunId("run".into());
        let bind = |mut command: Command| {
            command.run_id = Some(run.clone());
            command
        };
        let marker = bind(ActiveNativeAttemptCoverage::write(
            &NativeAttemptCoverage::JournalledV1,
        ));
        let mut model = ModelOperation::next(
            None,
            ModelBinding::parse("provider", "model", "genai").unwrap(),
        )
        .unwrap();
        model.effect_scope = ModelEffectScope::ResponseOnly;
        let response = bind(ActiveModelOperation::write(&Some(model.clone())));
        let mut snapshot = recovery_snapshot(
            "thread",
            vec![RunRecord {
                id: run.clone(),
                thread_id: ThreadId("thread".into()),
                state: RunState::Ended(EndCause::Indeterminate),
            }],
            vec![],
            vec![],
        );
        snapshot.state = vec![marker.clone(), response.clone()];
        assert!(
            response_only_run(&snapshot, &run),
            "R1 original response-only, including unknown model result"
        );
        snapshot.state = vec![marker.clone()];
        assert!(
            response_only_run(&snapshot, &run),
            "R1 covered pre-model exit"
        );
        for fault in 0..6 {
            snapshot.state = vec![marker.clone(), response.clone()];
            match fault {
                0 => {
                    snapshot.state.remove(0);
                }
                1 => {
                    model.effect_scope = ModelEffectScope::Opaque;
                    snapshot.state[1] = bind(ActiveModelOperation::write(&Some(model.clone())));
                }
                2 => {
                    snapshot.state[0] = bind(ActiveNativeAttemptCoverage::write(
                        &NativeAttemptCoverage::Unspecified,
                    ));
                }
                3 => {
                    let batch = ToolBatch::for_step(
                        run.clone(),
                        1,
                        [(
                            ToolCall {
                                call_id: "tool".into(),
                                tool_id: "read".into(),
                                arguments: serde_json::json!({}),
                            },
                            ToolRecoveryPolicy::default(),
                        )],
                    )
                    .unwrap();
                    snapshot
                        .state
                        .push(bind(ActiveToolBatch::write(&Some(batch))));
                }
                4 => {
                    snapshot.state.push(bind(ActiveToolBatch::write(&None)));
                }
                _ => {
                    snapshot.state[1].action = awaken_agent_contract::agent::state::Action::Set(
                        serde_json::json!({"invalid":true}),
                    );
                }
            }
            assert!(!response_only_run(&snapshot, &run), "R2 fault {fault}");
        }
    }

    #[tokio::test]
    async fn terminal_recovery_requires_every_frozen_thread_and_original_attempt() {
        use awaken_dispatch_contract::dispatch::PhysicalAttempt;
        use awaken_dispatch_contract::{
            AttemptClosureEvent, AttemptClosureKind, AttemptClosureRecord, AttemptPhysicalFact,
            DispatchCursor, DispatchOperation, DispatchOperationalEvent, RunClaim,
        };
        use awaken_session_execution_contract::{
            SessionExecutionRecoveryGap as Gap, SessionExecutionRecoveryObservation as Observation,
        };
        // Cause/effect graph: terminal fence F, complete frozen scope S, each
        // member's history H, complete entry inventory I, exact independent
        // closure C. R1 F+S+H+I+C -> closed effects, while cleanup stays Requested.
        // R2 !F -> not terminal; R3 F+!S -> scope unknown; R4 !H -> missing history;
        // R5 !I -> incomplete inventory; R6 !C -> old physical attempt stays open;
        // R7 record dependency unavailable -> unknown, never discard that member.
        // R8 foreign member snapshot or R9 foreign physical parent -> identity refusal.
        // R10 every member has original response-only/no-tool coverage ->
        // optional footprint. R11 one opaque child -> closure still truthful,
        // but no scope-wide no-effects footprint may borrow the root's proof.
        for rule in 1..=11 {
            let id = SessionId::from(format!("recovery-scope-{rule}"));
            let child = ThreadId(format!("{}-child", id.as_str()));
            let root_run = RunId(format!("{}-root-run", id.as_str()));
            let child_run = RunId(format!("{}-child-run", id.as_str()));
            let mut session = recovery_session(id.as_str());
            if rule != 2 {
                session.execution =
                    awaken_session_state_contract::SessionExecutionState::Terminated;
                session.ensure_terminal_cleanup_fence();
                if rule != 3 {
                    session
                        .freeze_terminal_cleanup_targets([child.0.clone()], 2, 41)
                        .unwrap();
                }
            }
            let runtime = Arc::new(RecoveryHistory::default());
            for (thread, run) in [
                (ThreadId(id.as_str().into()), root_run.clone()),
                (child.clone(), child_run.clone()),
            ] {
                if rule == 4 && thread == child {
                    continue;
                }
                let mut snapshot = recovery_snapshot(
                    &thread.0,
                    vec![RunRecord {
                        id: run,
                        thread_id: thread.clone(),
                        state: RunState::Ended(EndCause::Indeterminate),
                    }],
                    Vec::new(),
                    Vec::new(),
                );
                if rule == 8 && thread == child {
                    snapshot.thread_id = ThreadId("foreign-thread".into());
                }
                if rule >= 10 {
                    use awaken_agent_contract::agent::state::StateKey;
                    use awaken_runtime_contract::attempt_termination::{
                        ActiveNativeAttemptCoverage, NativeAttemptCoverage,
                    };
                    use awaken_runtime_contract::model_operation::{
                        ActiveModelOperation, ModelEffectScope, ModelOperation,
                    };
                    let owner = snapshot.runs[0].id.clone();
                    let mut marker =
                        ActiveNativeAttemptCoverage::write(&NativeAttemptCoverage::JournalledV1);
                    marker.run_id = Some(owner.clone());
                    let mut model = ModelOperation::next(
                        None,
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "provider", "model", "genai",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    model.effect_scope = if rule == 11 && thread == child {
                        ModelEffectScope::Opaque
                    } else {
                        ModelEffectScope::ResponseOnly
                    };
                    let mut entry = ActiveModelOperation::write(&Some(model));
                    entry.run_id = Some(owner);
                    snapshot.state = vec![marker, entry];
                    snapshot.state_commit_cursors = vec![1, 2];
                }
                runtime.set_recovery_snapshot(id.as_str(), &thread, snapshot);
            }
            let mut events = Vec::new();
            let mut closures = Vec::new();
            for (index, (thread, run)) in
                [(ThreadId(id.as_str().into()), root_run), (child, child_run)]
                    .into_iter()
                    .enumerate()
            {
                let mut entry = PhysicalAttempt {
                    claim: RunClaim {
                        run_id: run,
                        owner: format!("worker-{index}"),
                        epoch: 1,
                    },
                    binding: None,
                    thread_id: Some(thread),
                    session_thread_id: Some(ThreadId(id.as_str().into())),
                    closing: false,
                };
                if rule == 9 && index == 1 {
                    entry.session_thread_id = Some(ThreadId("foreign-session".into()));
                }
                if !(rule == 5 && index == 1) {
                    events.push(DispatchOperationalEvent {
                        cursor: DispatchCursor(index as u64 + 1),
                        recorded_at_ms: Some(1),
                        operation: DispatchOperation::AttemptAdmitted {
                            attempt: entry.clone(),
                        },
                    });
                }
                if !(rule == 6 && index == 1) {
                    let mut stopped = entry;
                    stopped.closing = true;
                    closures.push(AttemptClosureEvent {
                        cursor: DispatchCursor(index as u64 + 10),
                        recorded_at_ms: Some(2),
                        record: AttemptPhysicalFact::IndependentClosure {
                            record: AttemptClosureRecord::try_new(
                                stopped,
                                AttemptClosureKind::ControlledExecutionClosed,
                                format!("original-closure-{index}"),
                            )
                            .unwrap(),
                        },
                    });
                }
            }
            let feed = Arc::new(RecoverySource {
                events,
                closures: Some(closures),
                calls: 0.into(),
                unavailable: (rule == 7).into(),
            });
            let observed = read_execution_recovery(feed.as_ref(), runtime.as_ref(), &session).await;
            if matches!(rule, 1 | 10 | 11) {
                let Observation::EffectsClosed {
                    operation_footprint,
                    runtime_commit_cursor,
                    thread_ids,
                    closure_records,
                    ..
                } = observed
                else {
                    panic!("R1 complete scope is closed");
                };
                assert_eq!(runtime_commit_cursor, 41, "R1 original scope fence");
                assert_eq!(operation_footprint, (rule == 10).then_some(awaken_session_execution_contract::SessionExecutionOperationFootprint::ResponseOnly), "R10/R11 complete original scope only");
                assert_eq!(thread_ids.len(), 2, "R1 includes child");
                assert_eq!(
                    closure_records.len(),
                    2,
                    "R1 includes every original attempt"
                );
                assert!(
                    session.terminal_cleanup.is_requested(),
                    "R1 physical cleanup obligation stays pending"
                );
            } else {
                let reason = match rule {
                    2 => Gap::NotTerminal,
                    3 => Gap::ScopeNotFrozen,
                    4 => Gap::HistoryMissing,
                    5 => Gap::InventoryIncomplete,
                    6 => Gap::AttemptStillOpen,
                    7 => Gap::HistoryUnavailable,
                    _ => Gap::IdentityMismatch,
                };
                assert_eq!(observed, Observation::NotProven { reason }, "R{rule}");
            }
        }
    }
}
