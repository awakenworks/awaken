//! Committed Session histories built step by step, and the three properties
//! the Managed timeline must hold over every one of them.
//!
//! P1 schedule independence: for a history H and every refresh schedule S,
//! what a warm replica publishes after refreshing at S equals what a cold
//! replica builds from H's prefix alone, at every refreshed point.
//! P2 prefix monotonicity: for every prefix P of H and the next prefix P',
//! the cold history of P' keeps every id of P with the same payload and in
//! the same relative order.
//! P3 replica switch: for every id a warm replica published, a fresh cold
//! replica pages after that id to exactly the warm suffix.
//!
//! Each property returns the first violation instead of panicking, so a
//! scenario whose history still diverges is asserted to diverge (and must
//! leave its known-divergence list once fixed) rather than ignored.
//!
//! Consumers: warm_cold_equivalence.rs (P1), prefix_monotonicity.rs (P2) and
//! replica_switch.rs (P3).

use super::*;

pub(super) type DurableEvent = (String, serde_json::Value, Option<String>);

/// A committed history that can be extended one Runtime commit at a time and
/// read by a warm replica or by a fresh cold one.
pub(super) trait Timeline: Sized {
    type Step: Copy + std::fmt::Debug;

    async fn new(steps: &[Self::Step]) -> Self;
    async fn apply(&mut self, step: Self::Step);
    fn warm(&self) -> &ManagedState;
    fn session_id(&self) -> &str;
    /// A fresh replica over the same Runtime and Session stores.
    fn cold(&self) -> ManagedState;
}

pub(super) fn durable_history(state: &ManagedState, session_id: &str) -> Vec<DurableEvent> {
    durable_suffix(state, session_id, None).expect("the Session history is readable")
}

/// Every durable event after `after`, following each page to the end.
fn durable_suffix(
    state: &ManagedState,
    session_id: &str,
    after: Option<&str>,
) -> Result<Vec<DurableEvent>, StateError> {
    let mut suffix = Vec::new();
    let mut cursor = after.map(str::to_string);
    loop {
        let page = state.list_events(session_id, cursor.as_deref(), Some(100), false)?;
        suffix.extend(
            page.data
                .into_iter()
                .filter(|event| !is_transient_event_id(&event.id))
                .map(|event| {
                    (
                        event.id,
                        serde_json::to_value(&event.kind).unwrap(),
                        event.processed_at,
                    )
                }),
        );
        match page.next_page {
            Some(next) => cursor = Some(next),
            None => return Ok(suffix),
        }
    }
}

fn types(history: &[DurableEvent]) -> Vec<String> {
    history
        .iter()
        .map(|(_, kind, _)| kind["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The aggregate opens once per interval: a second `session.status_running`
/// before the aggregate closes would announce one interval twice.
fn one_aggregate_opening_per_interval(history: &[DurableEvent]) -> Result<(), String> {
    let mut open = false;
    for (id, kind, _) in history {
        match kind["type"].as_str().unwrap_or_default() {
            "session.status_running" if open => {
                return Err(format!("{id} opens an interval already open"));
            }
            "session.status_running" => open = true,
            "session.status_idle" | "session.status_terminated" => open = false,
            _ => {}
        }
    }
    Ok(())
}

/// Refresh one replica. A refresh that fails is a property violation: the
/// committed prefix did not project on that replica.
async fn refresh(state: &ManagedState, session_id: &str) -> Result<(), String> {
    state
        .refresh_committed_events(session_id)
        .await
        .map_err(|error| format!("the committed prefix does not project: {error}"))
}

/// P1 for one schedule: refresh warm at every step whose bit is set, compare
/// with a cold replica at each of those points and at the end. Returns the
/// final history.
pub(super) async fn warm_equals_cold<T: Timeline>(
    steps: &[T::Step],
    schedule: u64,
) -> Result<Vec<DurableEvent>, String> {
    let mut timeline = T::new(steps).await;
    for (position, step) in steps.iter().copied().enumerate() {
        timeline.apply(step).await;
        if schedule & (1 << position) != 0 {
            compare_with_cold(&timeline, &format!("schedule {schedule:#b} after {step:?}")).await?;
        }
    }
    compare_with_cold(&timeline, &format!("schedule {schedule:#b} final")).await
}

/// Refresh the warm replica, build a cold one over the same prefix, and
/// compare their durable histories.
async fn compare_with_cold<T: Timeline>(
    timeline: &T,
    label: &str,
) -> Result<Vec<DurableEvent>, String> {
    refresh(timeline.warm(), timeline.session_id())
        .await
        .map_err(|error| format!("{label}: warm {error}"))?;
    let warm = durable_history(timeline.warm(), timeline.session_id());
    let cold = timeline.cold();
    refresh(&cold, timeline.session_id())
        .await
        .map_err(|error| format!("{label}: cold {error}"))?;
    let cold = durable_history(&cold, timeline.session_id());
    if warm != cold {
        return Err(format!(
            "{label}: warm {:?} != cold {:?}",
            types(&warm),
            types(&cold)
        ));
    }
    one_aggregate_opening_per_interval(&warm).map_err(|error| format!("{label}: {error}"))?;
    Ok(warm)
}

/// P1 over every refresh schedule. Every schedule must also end on one
/// public sequence, which is returned.
pub(super) async fn every_schedule_equivalent<T: Timeline>(
    steps: &[T::Step],
) -> Result<Vec<String>, String> {
    assert!(
        steps.len() <= 8,
        "exhaustive schedules are bounded to 8 steps"
    );
    let mut finals = std::collections::BTreeSet::new();
    for schedule in 0..(1_u64 << steps.len()) {
        finals.insert(types(&warm_equals_cold::<T>(steps, schedule).await?));
    }
    let mut finals = finals.into_iter();
    let only = finals.next().expect("at least one schedule");
    match finals.next() {
        None => Ok(only),
        Some(other) => Err(format!(
            "schedules end on different sequences: {only:?} and {other:?}"
        )),
    }
}

/// P2: the cold history after each step keeps every id the cold history of
/// the previous step published, with its payload, in its relative order.
pub(super) async fn prefix_monotone<T: Timeline>(steps: &[T::Step]) -> Result<(), String> {
    let mut timeline = T::new(steps).await;
    let mut previous: Vec<DurableEvent> = Vec::new();
    for step in steps.iter().copied() {
        timeline.apply(step).await;
        let cold = timeline.cold();
        refresh(&cold, timeline.session_id())
            .await
            .map_err(|error| format!("after {step:?}: {error}"))?;
        let current = durable_history(&cold, timeline.session_id());
        let mut position = 0;
        for event in &previous {
            match current.iter().position(|candidate| candidate.0 == event.0) {
                None => {
                    return Err(format!(
                        "after {step:?}: {} {} was retracted",
                        event.0, event.1["type"]
                    ));
                }
                Some(found) if current[found] != *event => {
                    return Err(format!(
                        "after {step:?}: {} changed from {} to {}",
                        event.0, event.1, current[found].1
                    ));
                }
                Some(found) if found < position => {
                    return Err(format!(
                        "after {step:?}: {} {} moved before an event it followed",
                        event.0, event.1["type"]
                    ));
                }
                Some(found) => position = found,
            }
        }
        previous = current;
    }
    Ok(())
}

/// P3: warm refreshes after every step; a fresh cold replica then pages
/// after each warm id to the same suffix.
pub(super) async fn replica_switch_preserves_suffixes<T: Timeline>(
    steps: &[T::Step],
) -> Result<(), String> {
    let mut timeline = T::new(steps).await;
    for step in steps.iter().copied() {
        timeline.apply(step).await;
        refresh(timeline.warm(), timeline.session_id())
            .await
            .map_err(|error| format!("after {step:?}: warm {error}"))?;
    }
    let warm = durable_history(timeline.warm(), timeline.session_id());
    let cold = timeline.cold();
    refresh(&cold, timeline.session_id())
        .await
        .map_err(|error| format!("cold {error}"))?;
    for (index, (id, _, _)) in warm.iter().enumerate() {
        let suffix = durable_suffix(&cold, timeline.session_id(), Some(id))
            .map_err(|error| format!("cold replica cannot page after warm id {id}: {error}"))?;
        if suffix != warm[index + 1..] {
            return Err(format!(
                "after {id}: warm suffix {:?} != cold suffix {:?}",
                types(&warm[index + 1..]),
                types(&suffix)
            ));
        }
    }
    Ok(())
}

/// One committed boundary of a Session-root Run. Every step commits at its
/// own strictly increasing Runtime source cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Step {
    /// Retain the user Event with its anchor, admit the Session activity it
    /// owns, commit the input message and the Run's `Running` lifecycle.
    Input(usize),
    /// Admit a Session activity with no retained user Event (a profiled or
    /// Run-admission start) and commit its Run's `Running` lifecycle.
    Begin(usize),
    /// Run-port customer input without a Managed Event batch.
    ExternalInput(usize),
    /// Commit a Run's `Running` lifecycle with no Session activity, as a Run
    /// started before interval lineage or through a Runtime-only path does.
    Legacy(usize),
    /// Commit one assistant text response.
    Say(usize),
    /// Commit one executed tool call and its result.
    Tool(usize),
    /// Commit, in one commit, a response whose tool call follows more
    /// thinking blocks than the replica holds events, its result, and a
    /// further response.
    Burst(usize),
    /// Commit a tool call that awaits permission: its message, the answerable
    /// ticket, `Awaiting`, and the audit row naming the await target.
    Ask(usize),
    /// As [`Step::Ask`], but the commit left no audit of the await target.
    AskUnaudited(usize),
    /// Commit a scheduled action that pauses the Run while its Session
    /// activity stays admitted, with its audited await target.
    Schedule(usize),
    /// The ticket is consumed and the Run resumes: `Resumed` and the tool
    /// result commit together. After [`Step::Schedule`] this is the
    /// autonomous resumption; after an ask it is the answered one.
    Reply(usize),
    /// Commit a claim-fenced `Rescheduled` transition of the same Run.
    Reschedule(usize),
    /// Cancel the Run: the ticket (if any) is dropped, `Cancelled` commits
    /// and the Session activity settles.
    Cancel(usize),
    /// Commit the final response and `Completed`, and settle the Session
    /// activity if the Run has one.
    Close(usize),
}

struct RunSlot {
    run: RunId,
    epoch: Option<u64>,
    asks: usize,
}

/// Session-root histories over the root-only Runtime fixture.
pub(super) struct RootTimeline {
    runtime: LifecycleRuntime,
    repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    warm: ManagedState,
    session_id: String,
    cursor: u64,
    runs: Vec<Option<RunSlot>>,
}

impl RootTimeline {
    fn slot(&mut self, index: usize) -> &mut RunSlot {
        self.runs[index]
            .as_mut()
            .expect("a start step precedes the Run")
    }

    fn ask(&mut self, index: usize, cursor: u64, reason: ToolAwaitKind) {
        let session_id = self.session_id.clone();
        let slot = self.slot(index);
        let run = slot.run.clone();
        let call = format!("ask-{index}-{}", slot.asks);
        let message = MessageId::assistant(&run, 10 + slot.asks);
        let input = serde_json::json!({"path": "secret.txt"});
        self.runtime.push_committed_message(
            cursor,
            Message::new(
                message,
                Role::Assistant,
                vec![ContentBlock::tool_use(&call, "read", input.clone())],
            ),
        );
        *self.runtime.pending.lock().unwrap() = Some(Pending {
            tool_use_id: call.clone(),
            name: "read".into(),
            input: input.clone(),
            client_executed: false,
        });
        let target = awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
            reason: match reason {
                ToolAwaitKind::Permission | ToolAwaitKind::PermissionUnaudited => {
                    awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission
                }
                ToolAwaitKind::Scheduled => {
                    awaken_agent_contract::agent::awaiting::ToolAwaitReason::ScheduledAction
                }
            },
            call_id: call,
            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                tool_id: "read".into(),
                arguments: input,
            },
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        };
        let await_reason = target.reason();
        if reason == ToolAwaitKind::PermissionUnaudited {
            self.runtime
                .unaudited_awaits
                .lock()
                .unwrap()
                .insert(lifecycle_cursor(cursor));
        } else {
            let draft: awaken_agent_contract::audit::draft::Draft =
                awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                    state: RunState::Awaiting,
                    await_reason: Some(await_reason),
                    await_target: Some(target),
                }
                .into();
            self.runtime.audit_events.lock().unwrap().push(
                awaken_agent_contract::audit::record::Record {
                    sequence: lifecycle_cursor(cursor).0,
                    run_id: run.clone(),
                    kind: draft.kind,
                    payload: draft.payload,
                },
            );
        }
        let mut awaiting = lifecycle(
            cursor,
            &session_id,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        );
        awaiting.await_reason = Some(await_reason);
        self.runtime.lifecycle.lock().unwrap().push(awaiting);
    }

    async fn settle(&mut self, index: usize, cursor: u64) {
        let session = SessionId::from(&self.session_id);
        let thread = ThreadId(self.session_id.clone());
        let slot = self.slot(index);
        let (run, epoch) = (slot.run.clone(), slot.epoch);
        if let Some(epoch) = epoch {
            self.warm
                .application
                .settle_activity_observed(
                    &session,
                    epoch,
                    Some(
                        awaken_session_state_contract::SessionRuntimeIntervalObservation {
                            activity_epoch: epoch,
                            thread_id: thread,
                            run_id: run,
                            lifecycle_cursor: lifecycle_cursor(cursor),
                            source_commit_cursor: cursor,
                        },
                    ),
                )
                .await
                .unwrap();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolAwaitKind {
    Permission,
    PermissionUnaudited,
    Scheduled,
}

impl Timeline for RootTimeline {
    type Step = Step;

    async fn new(steps: &[Step]) -> Self {
        let runs = steps
            .iter()
            .map(|step| match *step {
                Step::Input(index)
                | Step::Begin(index)
                | Step::ExternalInput(index)
                | Step::Legacy(index)
                | Step::Say(index)
                | Step::Tool(index)
                | Step::Burst(index)
                | Step::Ask(index)
                | Step::AskUnaudited(index)
                | Step::Schedule(index)
                | Step::Reply(index)
                | Step::Reschedule(index)
                | Step::Cancel(index)
                | Step::Close(index) => index + 1,
            })
            .max()
            .unwrap_or_default();
        let runtime = LifecycleRuntime::default();
        let repository = Arc::new(ephemeral_session_repo());
        let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
        // These properties compare every refreshed point with a fresh cold
        // replica themselves, and a scenario still on a known-divergence list
        // is asserted to diverge; the per-publication oracle would stop it
        // at the first divergence instead.
        warm.disable_warm_cold_oracle();
        let session = warm
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap();
        Self {
            runtime,
            repository,
            warm,
            session_id: session.id,
            cursor: 0,
            runs: (0..runs).map(|_| None).collect(),
        }
    }

    fn warm(&self) -> &ManagedState {
        &self.warm
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn cold(&self) -> ManagedState {
        let cold =
            ManagedState::new(self.runtime.clone()).with_session_repo(self.repository.clone());
        cold.disable_warm_cold_oracle();
        cold
    }

    async fn apply(&mut self, step: Step) {
        let session = SessionId::from(&self.session_id);
        self.cursor += 1;
        let cursor = self.cursor;
        let session_id = self.session_id.clone();
        match step {
            Step::Input(index) => {
                let retained = self
                    .warm
                    .application
                    .append_session_event_batch(
                        &session_id,
                        vec![SessionEventInput::UserMessage {
                            content: vec![ContentBlock::text(format!("input-{index}"))],
                        }],
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                let SessionEventCommand::UserMessage { run_id, .. } = &retained.events[0].event
                else {
                    unreachable!("the retained batch holds the user message")
                };
                let run_id = run_id.clone();
                // Admission order matches production: the retained batch
                // owns the interval that its drive opens afterwards.
                let activity = self
                    .warm
                    .application
                    .begin_activity(&session)
                    .await
                    .unwrap();
                let operation_id = retained.events[0].event.operation_id().to_string();
                persist_batch_projection_anchors(
                    &self.warm,
                    &session_id,
                    &retained.batch_id,
                    &[cursor],
                )
                .await;
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId::session_event_input(&session_id, &operation_id),
                        Role::User,
                        vec![ContentBlock::text(format!("input-{index}"))],
                    ),
                );
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &session_id,
                    &run_id,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ));
                self.runs[index] = Some(RunSlot {
                    run: run_id,
                    epoch: Some(activity.activity_epoch),
                    asks: 0,
                });
            }
            Step::Begin(index) | Step::ExternalInput(index) | Step::Legacy(index) => {
                let epoch = match step {
                    Step::Begin(_) | Step::ExternalInput(_) => Some(
                        self.warm
                            .application
                            .begin_activity(&session)
                            .await
                            .unwrap()
                            .activity_epoch,
                    ),
                    _ => None,
                };
                let run_id = RunId(format!("begun-run-{index}"));
                if matches!(step, Step::ExternalInput(_)) {
                    self.runtime.push_committed_message(
                        cursor,
                        Message::text(
                            MessageId(format!("external-input-{index}")),
                            Role::User,
                            "服务费金额没提供；收款时间没定。",
                        ),
                    );
                }
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &session_id,
                    &run_id,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ));
                self.runs[index] = Some(RunSlot {
                    run: run_id,
                    epoch,
                    asks: 0,
                });
            }
            Step::Say(index) => {
                let run = self.slot(index).run.clone();
                self.runtime.push_committed_message(
                    cursor,
                    Message::text(MessageId::assistant(&run, 0), Role::Assistant, "working"),
                );
            }
            Step::Tool(index) => {
                let run = self.slot(index).run.clone();
                let call = format!("call-{index}");
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId::assistant(&run, 1),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            &call,
                            "read",
                            serde_json::json!({"path": "notes.txt"}),
                        )],
                    ),
                );
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId(format!("{}-tool-result", run.0)),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            &call,
                            vec![ContentBlock::text("contents")],
                        )],
                    ),
                );
            }
            Step::Burst(index) => {
                let run = self.slot(index).run.clone();
                let call = format!("burst-{index}");
                let mut blocks = (0..11)
                    .map(|step| ContentBlock::thinking(format!("step {step}")))
                    .collect::<Vec<_>>();
                blocks.push(ContentBlock::tool_use(
                    &call,
                    "read",
                    serde_json::json!({"path": "burst.txt"}),
                ));
                for message in [
                    Message::new(MessageId::assistant(&run, 3), Role::Assistant, blocks),
                    Message::new(
                        MessageId(format!("{}-burst-result", run.0)),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            &call,
                            vec![ContentBlock::text("burst")],
                        )],
                    ),
                    Message::text(MessageId::assistant(&run, 4), Role::Assistant, "after"),
                ] {
                    self.runtime.push_committed_message(cursor, message);
                }
            }
            Step::Ask(index) => self.ask(index, cursor, ToolAwaitKind::Permission),
            Step::AskUnaudited(index) => {
                self.ask(index, cursor, ToolAwaitKind::PermissionUnaudited);
            }
            Step::Schedule(index) => self.ask(index, cursor, ToolAwaitKind::Scheduled),
            Step::Reply(index) => {
                let slot = self.slot(index);
                let (run, asks) = (slot.run.clone(), slot.asks);
                slot.asks += 1;
                *self.runtime.pending.lock().unwrap() = None;
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId(format!("{}-ask-result-{asks}", run.0)),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            format!("ask-{index}-{asks}"),
                            vec![ContentBlock::text("secret")],
                        )],
                    ),
                );
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &session_id,
                    &run,
                    RunLifecycleEventKind::Resumed,
                    RunState::Running,
                ));
            }
            Step::Reschedule(index) => {
                let run = self.slot(index).run.clone();
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &session_id,
                    &run,
                    RunLifecycleEventKind::Rescheduled,
                    RunState::Running,
                ));
            }
            Step::Cancel(index) => {
                let run = self.slot(index).run.clone();
                *self.runtime.pending.lock().unwrap() = None;
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &session_id,
                    &run,
                    RunLifecycleEventKind::Cancelled,
                    RunState::Ended(EndCause::Cancelled),
                ));
                self.settle(index, cursor).await;
            }
            Step::Close(index) => {
                let run = self.slot(index).run.clone();
                self.runtime.push_committed_message(
                    cursor,
                    Message::text(MessageId::assistant(&run, 2), Role::Assistant, "done"),
                );
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &session_id,
                    &run,
                    RunLifecycleEventKind::Completed,
                    RunState::Ended(EndCause::NaturalEnd),
                ));
                self.settle(index, cursor).await;
            }
        }
    }
}

/// One committed boundary of one coordinated child Thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ChildStep {
    /// The root Run's accepted `send_message` links the child, whose Run
    /// commits `Running`.
    Link,
    /// Commit one child assistant response.
    Say,
    /// The child pauses on a permission ticket, with the audit row naming
    /// its await target when `audited`.
    Ask { audited: bool },
    /// The child's ticket is consumed: `Resumed` and the tool result commit.
    Reply,
    /// The child Run is cancelled.
    Cancel,
    /// The child commits its final response and `Completed`.
    Close,
}

const CHILD: &str = "sthr-timeline-child";

/// Histories of one coordinated child over the coordination fixture.
pub(super) struct ChildTimeline {
    runtime: RehydrateFake,
    repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    warm: ManagedState,
    session_id: String,
    cursor: u64,
    asks: usize,
}

impl ChildTimeline {
    fn replica(
        runtime: &RehydrateFake,
        repository: &Arc<awaken_session_store::SqliteManagedSessionRepository>,
    ) -> ManagedState {
        let state = ManagedState::new(runtime.clone())
            .with_session_repo(repository.clone())
            .with_config_source(Arc::new(FrozenToolFamilyProfiles::uniform(
                "coder",
                &["researcher"],
                FrozenTestToolFamily::AgentAlwaysAsk,
                "bash",
            )));
        state.disable_warm_cold_oracle();
        state
    }

    fn child_run() -> RunId {
        RunId("timeline-child-run".into())
    }

    fn commit_child_message(&self, cursor: u64, message: Message) {
        self.runtime
            .committed_by_thread
            .lock()
            .unwrap()
            .entry(CHILD.into())
            .or_default()
            .push(message);
        self.runtime
            .message_cursors_by_thread
            .lock()
            .unwrap()
            .entry(CHILD.into())
            .or_default()
            .push(cursor);
    }

    fn commit_child_lifecycle(&self, cursor: u64, kind: RunLifecycleEventKind, state: RunState) {
        self.runtime.lifecycle.lock().unwrap().push(lifecycle(
            cursor,
            CHILD,
            &Self::child_run(),
            kind,
            state,
        ));
    }
}

impl Timeline for ChildTimeline {
    type Step = ChildStep;

    async fn new(_steps: &[ChildStep]) -> Self {
        let runtime = RehydrateFake::default();
        let repository = Arc::new(ephemeral_session_repo());
        let warm = Self::replica(&runtime, &repository);
        let session = warm
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap();
        Self {
            runtime,
            repository,
            warm,
            session_id: session.id,
            cursor: 0,
            asks: 0,
        }
    }

    fn warm(&self) -> &ManagedState {
        &self.warm
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn cold(&self) -> ManagedState {
        Self::replica(&self.runtime, &self.repository)
    }

    async fn apply(&mut self, step: ChildStep) {
        self.cursor += 1;
        let cursor = self.cursor;
        let run = Self::child_run();
        match step {
            ChildStep::Link => {
                assert_eq!(cursor, 1, "the link is the history's first commit");
                let call = "timeline-link-call";
                self.runtime.install_agent_coordination_prefix(
                    &self.session_id,
                    RunId("timeline-root".into()),
                    0,
                    call,
                    CoordinatedThreadLink {
                        session_id: SessionId::from(&self.session_id),
                        thread_id: ThreadId(CHILD.into()),
                        target: CoordinatedThreadTarget::Agent {
                            agent_id: "researcher".into(),
                        },
                        created_by_operation_id: ToolBatch::operation_id_for_step(
                            &RunId("timeline-root".into()),
                            0,
                            call,
                        ),
                        latest_run_id: Some(run),
                    },
                );
                self.commit_child_lifecycle(
                    cursor,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                );
            }
            ChildStep::Say => self.commit_child_message(
                cursor,
                Message::text(MessageId::assistant(&run, 0), Role::Assistant, "child work"),
            ),
            ChildStep::Ask { audited } => {
                let call = format!("child-ask-{}", self.asks);
                let input = serde_json::json!({"command": format!("echo {}", self.asks)});
                self.commit_child_message(
                    cursor,
                    Message::new(
                        MessageId::assistant(&run, 10 + self.asks),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(&call, "bash", input.clone())],
                    ),
                );
                self.runtime.pending_by_thread.lock().unwrap().insert(
                    CHILD.into(),
                    Pending {
                        tool_use_id: call.clone(),
                        name: "bash".into(),
                        input: input.clone(),
                        client_executed: false,
                    },
                );
                let target = awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                    reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
                    call_id: call,
                    tool: awaken_agent_contract::agent::awaiting::PendingTool {
                        tool_id: "bash".into(),
                        arguments: input,
                    },
                    authority:
                        awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                };
                let mut awaiting = lifecycle(
                    cursor,
                    CHILD,
                    &run,
                    RunLifecycleEventKind::Awaiting,
                    RunState::Awaiting,
                );
                awaiting.await_reason = Some(target.reason());
                if audited {
                    self.runtime
                        .await_targets
                        .lock()
                        .unwrap()
                        .insert(lifecycle_cursor(cursor), target);
                } else {
                    self.runtime
                        .unaudited_awaits
                        .lock()
                        .unwrap()
                        .insert(lifecycle_cursor(cursor));
                }
                self.runtime.lifecycle.lock().unwrap().push(awaiting);
            }
            ChildStep::Reply => {
                let call = format!("child-ask-{}", self.asks);
                self.asks += 1;
                self.runtime.pending_by_thread.lock().unwrap().remove(CHILD);
                self.commit_child_message(
                    cursor,
                    Message::new(
                        MessageId(format!("child-ask-result-{call}")),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            &call,
                            vec![ContentBlock::text("approved")],
                        )],
                    ),
                );
                self.commit_child_lifecycle(
                    cursor,
                    RunLifecycleEventKind::Resumed,
                    RunState::Running,
                );
            }
            ChildStep::Cancel => {
                self.runtime.pending_by_thread.lock().unwrap().remove(CHILD);
                self.commit_child_lifecycle(
                    cursor,
                    RunLifecycleEventKind::Cancelled,
                    RunState::Ended(EndCause::Cancelled),
                );
            }
            ChildStep::Close => {
                self.commit_child_message(
                    cursor,
                    Message::text(MessageId::assistant(&run, 2), Role::Assistant, "child done"),
                );
                self.commit_child_lifecycle(
                    cursor,
                    RunLifecycleEventKind::Completed,
                    RunState::Ended(EndCause::NaturalEnd),
                );
            }
        }
    }
}

// The named histories every property checks. Each comment in the property
// files says why a history is shaped this way.
pub(super) const EXTERNAL_CUSTOMER_INPUT: &[Step] =
    &[Step::ExternalInput(0), Step::Say(0), Step::Close(0)];
pub(super) const ONE_RUN: &[Step] = &[Step::Input(0), Step::Say(0), Step::Tool(0), Step::Close(0)];
pub(super) const CONSECUTIVE_RUNS: &[Step] = &[
    Step::Input(0),
    Step::Say(0),
    Step::Tool(0),
    Step::Close(0),
    Step::Input(1),
    Step::Say(1),
    Step::Close(1),
];
pub(super) const AUDITED_PERMISSION_PAUSE: &[Step] =
    &[Step::Input(0), Step::Ask(0), Step::Reply(0), Step::Close(0)];
pub(super) const RESCHEDULED_RUN: &[Step] = &[Step::Begin(0), Step::Reschedule(0), Step::Close(0)];
pub(super) const OPEN_RESCHEDULED_INTERVAL: &[Step] = &[Step::Begin(0), Step::Reschedule(0)];
pub(super) const OPEN_USER_EVENT_INTERVAL: &[Step] = &[Step::Input(0), Step::Say(0), Step::Tool(0)];
pub(super) const RESCHEDULED_SECOND_INTERVAL: &[Step] = &[
    Step::Begin(0),
    Step::Close(0),
    Step::Begin(1),
    Step::Reschedule(1),
    Step::Close(1),
];
pub(super) const ONE_COMMIT_OF_SEVERAL_MESSAGES: &[Step] =
    &[Step::Input(0), Step::Burst(0), Step::Close(0)];
pub(super) const RUN_WITHOUT_ACTIVITY: &[Step] = &[Step::Legacy(0), Step::Say(0), Step::Close(0)];
pub(super) const CONSECUTIVE_RUNS_WITHOUT_ACTIVITY: &[Step] = &[
    Step::Legacy(0),
    Step::Close(0),
    Step::Legacy(1),
    Step::Say(1),
    Step::Close(1),
];
pub(super) const UNAUDITED_PERMISSION_PAUSE: &[Step] = &[
    Step::Input(0),
    Step::AskUnaudited(0),
    Step::Reply(0),
    Step::Close(0),
];
pub(super) const SCHEDULED_AUTONOMOUS_RESUME: &[Step] = &[
    Step::Begin(0),
    Step::Schedule(0),
    Step::Reply(0),
    Step::Close(0),
];
pub(super) const RUN_TERMINATING_SEVERAL_TIMES: &[Step] = &[
    Step::Legacy(0),
    Step::Ask(0),
    Step::Reply(0),
    Step::Ask(0),
    Step::Reply(0),
    Step::Close(0),
];
pub(super) const CANCELLED_PAUSE: &[Step] = &[Step::Input(0), Step::Ask(0), Step::Cancel(0)];
pub(super) const CANCELLED_PAUSE_WITHOUT_ACTIVITY: &[Step] =
    &[Step::Legacy(0), Step::Ask(0), Step::Cancel(0)];
pub(super) const CHILD_AUDITED_PAUSE: &[ChildStep] = &[
    ChildStep::Link,
    ChildStep::Ask { audited: true },
    ChildStep::Reply,
    ChildStep::Close,
];
pub(super) const CHILD_UNAUDITED_PAUSE: &[ChildStep] = &[
    ChildStep::Link,
    ChildStep::Ask { audited: false },
    ChildStep::Reply,
    ChildStep::Close,
];
pub(super) const CHILD_PAUSING_TWICE: &[ChildStep] = &[
    ChildStep::Link,
    ChildStep::Say,
    ChildStep::Ask { audited: true },
    ChildStep::Reply,
    ChildStep::Ask { audited: true },
    ChildStep::Reply,
    ChildStep::Close,
];
pub(super) const CHILD_CANCELLED_PAUSE: &[ChildStep] = &[
    ChildStep::Link,
    ChildStep::Ask { audited: true },
    ChildStep::Cancel,
];

/// Every named Session-root history.
pub(super) const ROOT_SCENARIOS: &[(&str, &[Step])] = &[
    ("one_run", ONE_RUN),
    ("external_customer_input", EXTERNAL_CUSTOMER_INPUT),
    ("consecutive_runs", CONSECUTIVE_RUNS),
    ("audited_permission_pause", AUDITED_PERMISSION_PAUSE),
    ("rescheduled_run", RESCHEDULED_RUN),
    ("open_rescheduled_interval", OPEN_RESCHEDULED_INTERVAL),
    ("open_user_event_interval", OPEN_USER_EVENT_INTERVAL),
    ("rescheduled_second_interval", RESCHEDULED_SECOND_INTERVAL),
    (
        "one_commit_of_several_messages",
        ONE_COMMIT_OF_SEVERAL_MESSAGES,
    ),
    ("run_without_activity", RUN_WITHOUT_ACTIVITY),
    (
        "consecutive_runs_without_activity",
        CONSECUTIVE_RUNS_WITHOUT_ACTIVITY,
    ),
    ("unaudited_permission_pause", UNAUDITED_PERMISSION_PAUSE),
    ("scheduled_autonomous_resume", SCHEDULED_AUTONOMOUS_RESUME),
    (
        "run_terminating_several_times",
        RUN_TERMINATING_SEVERAL_TIMES,
    ),
    ("cancelled_pause", CANCELLED_PAUSE),
    (
        "cancelled_pause_without_activity",
        CANCELLED_PAUSE_WITHOUT_ACTIVITY,
    ),
];

/// Every named coordinated-child history.
pub(super) const CHILD_SCENARIOS: &[(&str, &[ChildStep])] = &[
    ("child_audited_pause", CHILD_AUDITED_PAUSE),
    ("child_unaudited_pause", CHILD_UNAUDITED_PAUSE),
    ("child_pausing_twice", CHILD_PAUSING_TWICE),
    ("child_cancelled_pause", CHILD_CANCELLED_PAUSE),
];
