//! The sole warm/cold projection from committed Session, Thread, Run, and Outcome truth.

use super::*;

impl ManagedState {
    /// Project committed per-Run observations from the same recovery prefix as
    /// messages, lifecycle and resume tickets. This is the sole source of
    /// model-request spans and context-compaction markers; live Step results
    /// carry neither field.
    pub(super) fn append_run_observation_projections(
        record: &mut SessionRecord,
        thread_id: &str,
        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<(), StateError> {
        use awaken_runtime_contract::compaction::RunCompactionMarker;
        use awaken_runtime_contract::llm::ModelRequestObservation;

        for run in &snapshot.runs {
            if RunCompactionMarker::is_recorded(&snapshot.state, &run.id.0) {
                let id = managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    "run-context-compacted",
                    ManagedMultiagentEventProvenance::RunState { run_id: &run.id.0 },
                );
                if !record.events.iter().any(|event| event.id == id) {
                    if thread_id != record.session.id {
                        record
                            .event_thread_owners
                            .insert(id.clone(), thread_id.to_string());
                    }
                    record.events.push(Event {
                        id,
                        kind: OutboundKind::ThreadContextCompacted {},
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
            }

            for audit in snapshot
                .events
                .iter()
                .filter(|audit| audit.run_id == run.id)
            {
                let Some(observation) =
                    ModelRequestObservation::from_record(audit).map_err(|error| {
                        StateError::Run(RunError::internal(format!(
                            "decode committed model request observation: {error}"
                        )))
                    })?
                else {
                    continue;
                };
                let provenance = || ManagedMultiagentEventProvenance::Audit {
                    run_id: &run.id.0,
                    sequence: audit.sequence,
                };
                let start_id = managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    "model-request-start",
                    provenance(),
                );
                let end_id = managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    "model-request-end",
                    provenance(),
                );
                if record.events.iter().any(|event| event.id == end_id) {
                    continue;
                }
                if thread_id != record.session.id {
                    record
                        .event_thread_owners
                        .insert(start_id.clone(), thread_id.to_string());
                    record
                        .event_thread_owners
                        .insert(end_id.clone(), thread_id.to_string());
                }
                record.events.extend([
                    Event {
                        id: start_id.clone(),
                        kind: OutboundKind::SpanModelRequestStart {},
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                    Event {
                        id: end_id,
                        kind: OutboundKind::SpanModelRequestEnd {
                            model_request_start_id: start_id,
                            is_error: Some(observation.is_error),
                            model_usage: SpanModelUsage {
                                input_tokens: observation.usage.prompt_tokens,
                                output_tokens: observation.usage.completion_tokens,
                                cache_read_input_tokens: observation.usage.cache_read_tokens,
                                cache_creation_input_tokens: observation
                                    .usage
                                    .cache_creation_tokens,
                                speed: None,
                            },
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                ]);
            }
        }
        Ok(())
    }
    /// Refresh this process's disposable event projection from the Runtime's one
    /// durable transcript and Run lifecycle feed. A Session cache hit is not proof
    /// that it contains commits accepted through another protocol or Coordinator.
    pub(crate) async fn refresh_committed_events(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        self.ensure_session_record(session_id).await?;
        self.refresh_committed_projection(session_id).await
    }

    /// Sole warm/cold Runtime -> Managed projector entrypoint.  Rehydration calls
    /// this after creating only a base record; ordinary reads call it on every
    /// cache hit.  All external reads finish before the disposable record lock is
    /// taken, so no Runtime call is made while holding protocol state.
    pub(in crate::state) async fn refresh_committed_projection(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        // Private Worker realization mutates the same durable Session application
        // without passing through this protocol adapter. Refresh its disposable
        // wire projection before reading runtime events so GET cannot retain a
        // stale preparing/idle status as a parallel lifecycle authority.
        let persisted = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        let persisted_status = Self::wire_session_status(persisted.execution);
        let price_snapshot = persisted.budget.price_snapshot().cloned();
        let budget_reach_projections = persisted
            .budget
            .reach_transitions()
            .iter()
            .map(|transition| {
                let mut usage = session_usage_value(
                    transition
                        .usage_cursor
                        .to_session_usage()
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?,
                    None,
                )
                .map_err(StateError::Run)?;
                usage.list_cost = transition.public_list_cost_minor().map(|amount| {
                    crate::types::MonetaryAmount {
                        amount: amount.to_string(),
                        currency: crate::types::Currency::USD,
                    }
                });
                Ok((
                    transition.generation,
                    usage,
                    crate::types::BudgetLimit::Limit {
                        max_list_cost: crate::types::MonetaryAmount {
                            amount: transition.max_list_cost_minor.to_string(),
                            currency: crate::types::Currency::USD,
                        },
                    },
                ))
            })
            .collect::<Result<Vec<_>, StateError>>()?;
        let mut durable_outcome_projections = Vec::new();
        for outcome_id in retained_outcome_ids(&persisted.event_batches) {
            if let Some(terminal) = self
                .application
                .committed_outcome_projection(session_id, &outcome_id)
                .await
                .map_err(StateError::Run)?
            {
                durable_outcome_projections
                    .push(validate_durable_outcome_projection(outcome_id, terminal)?);
            }
        }
        self.refresh_cached_projection(&persisted)?;
        let initial_cursor = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .ok_or(StateError::NotFound)?
            .projected_lifecycle_cursor;
        // Read the root fence before the later relationship snapshot. This
        // ordering makes an unknown lifecycle event classifiable without
        // guessing from its Thread id: a relationship read taken after the
        // root fence must include every coordinated link committed at or before
        // that fence. An event beyond the fence remains unconsumed until the
        // next refresh, when its matching root/link snapshot can catch up.
        let root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        let mut links = self
            .application
            .coordinated_threads(session_id)
            .await
            .map_err(StateError::Run)?;
        let child_snapshots = self
            .coordinated_recovery_snapshots(session_id, &links)
            .await?;
        for link in &mut links {
            if let Some(snapshot) = child_snapshots.get(&link.thread_id.0) {
                link.latest_run_id = snapshot.latest_run_id.clone();
            }
        }
        // The lifecycle feed may advance after the snapshots. Each event carries
        // its source commit cursor, so the recovery snapshot's store cursor can
        // fence it to the exact prefix whose transcript, ticket and disposition
        // were read atomically. A terminal can therefore never close SSE before
        // that same prefix exposes its final output; opaque lifecycle event
        // cursors are never compared with commit-sequence cursors.
        const LIFECYCLE_PAGE_SIZE: usize = 256;
        let mut read_cursor = initial_cursor;
        let mut lifecycle_events = Vec::new();
        loop {
            let page = self
                .application
                .committed_run_lifecycle(session_id, read_cursor, LIFECYCLE_PAGE_SIZE)
                .await
                .map_err(StateError::Run)?;
            let count = page.events.len();
            lifecycle_events.extend(page.events);
            if page.next_cursor == read_cursor || count < LIFECYCLE_PAGE_SIZE {
                break;
            }
            read_cursor = page.next_cursor;
        }
        let mut transcripts = child_snapshots
            .iter()
            .map(|(thread_id, snapshot)| (thread_id.clone(), snapshot.messages.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        let child_latest_run_states = child_snapshots
            .iter()
            .filter_map(|(thread_id, snapshot)| {
                let latest_run_id = snapshot.latest_run_id.as_ref()?;
                snapshot
                    .runs
                    .iter()
                    .find(|run| &run.id == latest_run_id)
                    .map(|run| (thread_id.clone(), run.state.clone()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let mut child_pending = std::collections::HashMap::new();
        for (thread_id, snapshot) in &child_snapshots {
            if let Some(pending) = Self::pending_from_recovery_snapshot(snapshot)? {
                child_pending.insert(thread_id.clone(), pending);
            }
        }
        let child_dispositions = child_snapshots
            .iter()
            .map(|(thread_id, snapshot)| {
                awaken_agent_contract::thread_disposition_from_committed_state(&snapshot.state)
                    .map(|disposition| (thread_id.clone(), disposition))
                    .map_err(|error| {
                        StateError::Run(RunError::internal(format!(
                            "recover coordinated Thread disposition: {error}"
                        )))
                    })
            })
            .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
        let root_pending = root_snapshot
            .as_ref()
            .map(Self::pending_from_recovery_snapshot)
            .transpose()?
            .flatten();
        let messages = root_snapshot
            .as_ref()
            .map(|snapshot| snapshot.messages.clone())
            .unwrap_or_default();
        let root_run_ids = root_snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .runs
                    .iter()
                    .map(|run| run.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let root_response_coordinates =
            Self::assistant_response_coordinates(&messages, &root_run_ids);
        let durable_inbound_projections =
            durable_inbound_projections(session_id, &persisted.event_batches);
        let usage = self
            .application
            .session_usage(session_id)
            .await
            .map_err(StateError::Run)?;
        let primary_thread_usage = self
            .application
            .session_thread_usage(session_id, session_id)
            .await
            .map_err(StateError::Run)?;
        let primary_thread_usage =
            session_thread_usage_value(primary_thread_usage, price_snapshot.as_ref())
                .map_err(StateError::Run)?;
        let mut child_thread_usage = std::collections::HashMap::new();
        for link in &links {
            let usage = self
                .application
                .session_thread_usage(session_id, &link.thread_id.0)
                .await
                .map_err(StateError::Run)?;
            let usage = session_thread_usage_value(usage, price_snapshot.as_ref())
                .map_err(StateError::Run)?;
            child_thread_usage.insert(link.thread_id.0.clone(), usage);
        }

        let known_children = links
            .iter()
            .map(|link| link.thread_id.0.as_str())
            .collect::<std::collections::HashSet<_>>();
        let root_store_cursor = root_snapshot.as_ref().map(|snapshot| snapshot.store_cursor);
        let snapshot_for_thread = |thread_id: &str| {
            if thread_id == session_id {
                root_snapshot.as_ref()
            } else {
                child_snapshots.get(thread_id)
            }
        };
        let latest_cursor_by_thread = lifecycle_events
            .iter()
            .filter(|event| {
                snapshot_for_thread(&event.thread_id.0)
                    .is_some_and(|snapshot| event.source_commit_cursor <= snapshot.store_cursor)
            })
            .fold(std::collections::HashMap::new(), |mut latest, event| {
                latest.insert(event.thread_id.0.clone(), event.cursor);
                latest
            });
        let mut accepted_cursor = initial_cursor;
        let mut accepted_lifecycle = Vec::new();
        let mut blocked_transcript_threads = std::collections::HashSet::new();
        for event in lifecycle_events {
            let thread_id = event.thread_id.0.as_str();
            let is_root = thread_id == session_id;
            let is_known_child = known_children.contains(thread_id);
            if !is_root && !is_known_child {
                // The relationship read happened after `root_store_cursor`.
                // Therefore an event inside that prefix which still has no
                // relationship is out-of-scope lifecycle (another Postgres
                // Session or invisible auxiliary work) and can advance the
                // feed cursor without creating a public Thread. An event beyond
                // the prefix may be a just-created child whose link/snapshot is
                // not visible yet, so it remains replayable. Thread-id spelling
                // is deliberately never used as provenance.
                if root_store_cursor.is_some_and(|cursor| event.source_commit_cursor <= cursor) {
                    accepted_cursor = event.cursor;
                    continue;
                }
                break;
            }
            let snapshot_is_behind = snapshot_for_thread(thread_id)
                .is_none_or(|snapshot| event.source_commit_cursor > snapshot.store_cursor);
            if snapshot_is_behind {
                break;
            }
            let is_current_awaiting = event.kind == RunLifecycleEventKind::Awaiting
                && latest_cursor_by_thread.get(thread_id) == Some(&event.cursor);
            let has_pending_ticket = if is_root {
                root_pending.is_some()
            } else {
                child_pending.contains_key(thread_id)
            };
            let is_budget_pause = event.await_reason.as_ref()
                == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
            // A current Awaiting transition cannot be published without its
            // exact answerable ticket. Historical Awaiting transitions may be
            // followed by a committed resume and therefore need no live ticket.
            if is_current_awaiting && !has_pending_ticket && !is_budget_pause {
                // Transcript and lifecycle are one public classification. If the
                // message were marked projected before its ticket became visible,
                // a ToolUse would be irreversibly downgraded to allow and could
                // never become the answerable requires-action event.
                blocked_transcript_threads.insert(thread_id.to_string());
                break;
            }
            accepted_cursor = event.cursor;
            accepted_lifecycle.push(event);
        }
        for thread_id in &blocked_transcript_threads {
            transcripts.remove(thread_id);
        }

        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let start = record.events.len();
        merge_durable_inbound_projections(record, durable_inbound_projections);
        record.primary_thread_usage = primary_thread_usage;
        let unseen_budget_reach = budget_reach_projections
            .iter()
            .filter(|(generation, _, _)| *generation > record.projected_budget_reach_generation)
            .cloned()
            .collect::<Vec<_>>();
        let current_cursor = record.projected_lifecycle_cursor;
        accepted_lifecycle.retain(|event| event.cursor > current_cursor);
        if accepted_cursor > record.projected_lifecycle_cursor {
            record.projected_lifecycle_cursor = accepted_cursor;
        }
        let root_pending = root_pending.as_ref();
        let transcript_evidence = project::committed_transcript_evidence(&messages, root_pending);
        let new_messages = if blocked_transcript_threads.contains(session_id) {
            Vec::new()
        } else {
            messages
                .iter()
                .filter(|message| !record.message_was_projected(session_id, &message.id.0))
                .cloned()
                .collect::<Vec<_>>()
        };
        let root_terminal = accepted_lifecycle.iter().rev().find(|event| {
            event.thread_id.0 == session_id
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Awaiting
                        | RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                )
        });
        let unseen_root_terminal = root_terminal
            .filter(|event| !record.projected_terminal_cursors.contains(&event.cursor));
        let aggregate_running_transition =
            Self::should_append_aggregate_running(record, persisted_status);
        if aggregate_running_transition {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionStatusRunning {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // A warm reader or a restarted replica may first observe a Run only
        // after its terminal commit and the aggregate has already returned to
        // Idle. Reconstruct the missing opening edge before projecting output;
        // otherwise cold recovery yields agent output before
        // `session.status_running`, unlike the ordinary foreground path. The
        // terminal lifecycle cursor plus the aggregate Event tail are the
        // existing idempotency authorities, so no create-specific marker is
        // needed. An active child would keep the durable aggregate Running; an
        // Idle root is therefore already the closed-interval case.
        if unseen_root_terminal.is_some()
            && persisted_status == SessionStatus::Idle
            && !aggregate_running_transition
            && Self::needs_aggregate_running_edge(record)
        {
            let id = if links.is_empty() {
                self.next_event_id()
            } else {
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-terminal-status-running",
                    ManagedMultiagentEventProvenance::LifecyclePrefix {
                        cursor: record.projected_lifecycle_cursor.0,
                    },
                )
            };
            record.events.push(Event {
                id,
                kind: OutboundKind::SessionStatusRunning {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // Root Thread lifecycle is projected from the same accepted lifecycle
        // prefix as the aggregate Session edges. A foreground command already
        // inserted the bracket when its terminal cursor is present in the one
        // terminal set; otherwise warm/cold recovery emits every newly accepted
        // Running/Rescheduled edge. If a cold prefix starts at a terminal, the
        // event tail reconstructs the missing opening edge before output.
        let project_root_transitions = root_terminal.is_none() || unseen_root_terminal.is_some();
        let mut projected_root_opening = false;
        if project_root_transitions {
            for lifecycle in accepted_lifecycle.iter().filter(|event| {
                event.thread_id.0 == session_id
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Running | RunLifecycleEventKind::Rescheduled
                    )
            }) {
                match lifecycle.kind {
                    RunLifecycleEventKind::Running => {
                        let status = primary_thread_status_event(
                            record,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-lifecycle-status-running",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            PrimaryThreadStatusProjection::Running,
                        );
                        record.events.push(status);
                        projected_root_opening = true;
                    }
                    RunLifecycleEventKind::Rescheduled => {
                        let session_rescheduled = Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-lifecycle-session-status-rescheduled",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            kind: OutboundKind::SessionStatusRescheduled {},
                            processed_at: Some(PROCESSED_AT.to_string()),
                        };
                        let thread_rescheduled = primary_thread_status_event(
                            record,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-lifecycle-status-rescheduled",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            PrimaryThreadStatusProjection::Rescheduled,
                        );
                        let replacement_running = primary_thread_status_event(
                            record,
                            managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "root-rescheduled-lifecycle-status-running",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: lifecycle.cursor.0,
                                },
                            ),
                            PrimaryThreadStatusProjection::Running,
                        );
                        record.events.extend([
                            session_rescheduled,
                            thread_rescheduled,
                            replacement_running,
                        ]);
                        projected_root_opening = true;
                    }
                    _ => unreachable!("filtered root lifecycle transition"),
                }
            }
        }
        if !projected_root_opening
            && !Self::primary_thread_status_is_open(record)
            && let Some(terminal) = unseen_root_terminal
        {
            let reconstructed = primary_thread_status_event(
                record,
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "root-terminal-status-running",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                ),
                PrimaryThreadStatusProjection::Running,
            );
            record.events.push(reconstructed);
        }
        if !blocked_transcript_threads.contains(session_id)
            && let Some(snapshot) = root_snapshot.as_ref()
        {
            Self::append_run_observation_projections(record, session_id, snapshot)?;
        }
        let root_pending_source_run_id = root_terminal
            .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
            .map(|event| &event.run_id);
        let mut pending_is_in_new_message = false;
        for message in &new_messages {
            let tool_index = record.projected_tool_index_for(None);
            let selected = transcript_evidence.project_message(
                message,
                &tool_index.sources,
                root_pending_source_run_id.map(|run_id| run_id.0.as_str()),
            );
            pending_is_in_new_message |= selected.pending_occurrence;
            let message_pending = selected
                .retained_pending_occurrence
                .then_some(root_pending)
                .flatten();
            let mut projected = project_messages_with_mcp_ids(
                std::slice::from_ref(&selected.message),
                message_pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
            );
            self.qualify_tool_projection(
                record,
                None,
                std::slice::from_ref(&selected.message),
                root_pending_source_run_id,
                &mut projected,
            )?;
            for (ordinal, projected) in projected.into_iter().enumerate() {
                let kind = projected.kind;
                let id = projected.id.unwrap_or_else(|| {
                    match (&kind, root_response_coordinates.get(&message.id.0)) {
                        (
                            OutboundKind::AgentMessage { .. } | OutboundKind::AgentThinking {},
                            Some((run_id, step, response)),
                        ) => managed_assistant_event_id(
                            &record.session.id,
                            session_id,
                            run_id,
                            *step,
                            *response,
                            kind.type_str(),
                        ),
                        _ => managed_multiagent_event_id(
                            &record.session.id,
                            session_id,
                            kind.type_str(),
                            ManagedMultiagentEventProvenance::Message {
                                message_id: &message.id.0,
                                ordinal,
                            },
                        ),
                    }
                });
                if record.events.iter().any(|event| event.id == id) {
                    continue;
                }
                record.events.push(Event {
                    id,
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            if selected.fully_classified {
                record.consume_message(session_id, &message.id.0);
            }
        }
        // A committed ticket may precede its ToolUse transcript block. Retain
        // the existing synthetic path, qualified by the Awaiting Run, but only
        // when no revisitable source Message already owns that occurrence.
        if root_pending.is_some() && !pending_is_in_new_message {
            let tool_index = record.projected_tool_index_for(None);
            let mut projected = project_messages_with_mcp_ids(
                &[],
                root_pending,
                &tool_index.all,
                tool_index.mcp,
                transcript_evidence.advisor_blocks(),
            );
            self.qualify_tool_projection(
                record,
                None,
                &[],
                root_pending_source_run_id,
                &mut projected,
            )?;
            for projected in projected {
                let id = projected.id.ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "synthetic root pending projection has no qualified tool id",
                    ))
                })?;
                if !record.events.iter().any(|event| event.id == id) {
                    record.events.push(Event {
                        id,
                        kind: projected.kind,
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
            }
        }

        // Outcome observation is an immediate, side-effect-free committed fact.
        // The retained DefineOutcome selects the aggregate: Completed adds only
        // evaluation facts, while Errored adds one infrastructure error and no
        // partial evaluation. General transcript/lifecycle projection remains
        // the sole owner unless an exact failure source Run is correlated below.
        append_durable_outcome_projections(record, &durable_outcome_projections);

        self.append_delegation_projections(
            record,
            DelegationProjectionEvidence {
                links: &links,
                snapshots: &child_snapshots,
                transcripts: &transcripts,
                lifecycle_events: &accepted_lifecycle,
                latest_run_states: &child_latest_run_states,
                pending: &child_pending,
                dispositions: &child_dispositions,
                usage: &child_thread_usage,
            },
        )?;

        let child_terminal = accepted_lifecycle.iter().rev().find(|event| {
            known_children.contains(event.thread_id.0.as_str())
                && child_dispositions.get(event.thread_id.0.as_str())
                    != Some(&awaken_agent_contract::ThreadDisposition::Archived)
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Awaiting
                        | RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                )
        });
        let child_terminal_reached_budget = child_terminal.is_some_and(|event| {
            event.kind == RunLifecycleEventKind::Awaiting
                && event.await_reason.as_ref()
                    == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached)
        });
        let active_child = record.child_threads.iter().any(|child| {
            matches!(
                child.status,
                SessionThreadStatus::Running | SessionThreadStatus::Rescheduling
            )
        });
        let can_idle = persisted_status == SessionStatus::Idle && !active_child;
        let aggregate_budget_reached = !persisted.budget.can_admit_model_request();
        if let Some(terminal) = unseen_root_terminal {
            let thread_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                root_pending,
                terminal.await_reason.as_ref(),
            )?;
            let aggregate_reason = match thread_reason {
                reason @ StopReason::RequiresAction { .. } | reason @ StopReason::BudgetReached => {
                    reason
                }
                _ if aggregate_budget_reached => StopReason::BudgetReached,
                reason => reason,
            };
            // A scheduled action can commit Awaiting while the Session activity
            // is still Running, then autonomously resume and commit a newer
            // terminal for the same root Run before the aggregate may idle. The
            // newer same-Run reason supersedes that disposable continuation;
            // BudgetReached remains absorbing across every later terminal.
            let replace = match record.deferred_session_stop_reason.as_ref() {
                None => true,
                Some((_, StopReason::BudgetReached)) => false,
                Some((Some(owner), _)) => owner == &terminal.run_id,
                Some((None, _)) => false,
            };
            if replace || matches!(aggregate_reason, StopReason::BudgetReached) {
                record.deferred_session_stop_reason =
                    Some((Some(terminal.run_id.clone()), aggregate_reason));
            }
        }
        if !can_idle && let Some(terminal) = child_terminal {
            let reason = if child_terminal_reached_budget {
                StopReason::BudgetReached
            } else {
                Self::public_run_stop_reason(
                    record,
                    Some(terminal.thread_id.0.as_str()),
                    &terminal.state,
                    child_pending.get(terminal.thread_id.0.as_str()),
                    terminal.await_reason.as_ref(),
                )?
            };
            if matches!(reason, StopReason::BudgetReached) {
                record.deferred_session_stop_reason = Some((None, reason));
            } else {
                record
                    .deferred_session_stop_reason
                    .get_or_insert((None, reason));
            }
        }
        if can_idle && child_terminal_reached_budget {
            record.deferred_session_stop_reason = Some((None, StopReason::BudgetReached));
        }
        if let Some(terminal) = unseen_root_terminal {
            record.projected_terminal_cursors.insert(terminal.cursor);
            if let awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Error(failure),
            ) = &terminal.state
                && !durable_outcome_projections
                    .iter()
                    .any(|projection| projection.owns_failure_run(&terminal.run_id))
            {
                record.events.push(Event {
                    id: if links.is_empty() {
                        self.next_event_id()
                    } else {
                        managed_multiagent_event_id(
                            &record.session.id,
                            &record.session.id,
                            "root-lifecycle-error",
                            ManagedMultiagentEventProvenance::Lifecycle {
                                cursor: terminal.cursor.0,
                            },
                        )
                    },
                    kind: OutboundKind::SessionError {
                        error: SessionError::classify(failure.code(), failure.message()),
                    },
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            let stop_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                root_pending,
                terminal.await_reason.as_ref(),
            )?;
            let thread_idle = primary_thread_status_event(
                record,
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "root-lifecycle-status-idle",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: terminal.cursor.0,
                    },
                ),
                PrimaryThreadStatusProjection::Idle { stop_reason },
            );
            record.events.push(thread_idle);
        }
        let projected_usage =
            session_usage_value(usage, price_snapshot.as_ref()).map_err(StateError::Run)?;
        record.session.usage = projected_usage.clone();
        let mut pending_event_ids = Vec::new();
        if let Some(pending) = root_pending {
            pending_event_ids.push(
                record
                    .projected_tool_index_for(None)
                    .latest
                    .get(&pending.tool_use_id)
                    .cloned()
                    .ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "root pending ticket has no public Managed tool event",
                        ))
                    })?,
            );
        }
        for (thread_id, pending) in &child_pending {
            pending_event_ids.push(
                record
                    .projected_tool_index_for(Some(thread_id))
                    .latest
                    .get(&pending.tool_use_id)
                    .cloned()
                    .ok_or_else(|| {
                        StateError::Run(RunError::internal(
                            "child pending ticket has no public Managed tool event",
                        ))
                    })?,
            );
        }
        pending_event_ids.sort();
        pending_event_ids.dedup();
        if can_idle
            && (unseen_root_terminal.is_some()
                || child_terminal.is_some()
                || record.deferred_session_stop_reason.is_some()
                || (!unseen_budget_reach.is_empty() && pending_event_ids.is_empty()))
        {
            if pending_event_ids.is_empty() && !unseen_budget_reach.is_empty() {
                for (generation, usage, budget) in unseen_budget_reach {
                    record.events.extend([
                        Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "budget-reach-usage",
                                ManagedMultiagentEventProvenance::BudgetReach { generation },
                            ),
                            kind: OutboundKind::SessionUsage {
                                usage,
                                budget: Some(budget),
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        },
                        Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                &record.session.id,
                                "budget-reach-status-idle",
                                ManagedMultiagentEventProvenance::BudgetReach { generation },
                            ),
                            kind: OutboundKind::SessionStatusIdle {
                                stop_reason: StopReason::BudgetReached,
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        },
                    ]);
                    record.projected_budget_reach_generation = generation;
                }
            } else {
                let stop_reason = if pending_event_ids.is_empty() {
                    record
                        .deferred_session_stop_reason
                        .clone()
                        .map(|(_, reason)| reason)
                        .unwrap_or_else(|| {
                            root_terminal.map_or(StopReason::EndTurn, |event| {
                                Self::child_stop_reason(event, None)
                            })
                        })
                } else {
                    StopReason::RequiresAction {
                        event_ids: pending_event_ids,
                    }
                };
                // Only a terminal lifecycle prefix not yet represented by an
                // aggregate idle is replayable here. The shared terminal-cursor
                // set covers root and children.
                let multiagent_id = (!links.is_empty()
                    && record
                        .projected_terminal_cursors
                        .contains(&record.projected_lifecycle_cursor))
                .then(|| {
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-status-idle",
                        ManagedMultiagentEventProvenance::LifecyclePrefix {
                            cursor: record.projected_lifecycle_cursor.0,
                        },
                    )
                })
                .filter(|id| !record.events.iter().any(|event| event.id == *id));
                record.events.extend([
                    Event {
                        id: self.next_event_id(),
                        kind: OutboundKind::SessionUsage {
                            usage: projected_usage,
                            budget: record.session.budget.clone(),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                    Event {
                        id: multiagent_id.unwrap_or_else(|| self.next_event_id()),
                        kind: OutboundKind::SessionStatusIdle { stop_reason },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                ]);
            }
            record.deferred_session_stop_reason = None;
        }
        // Parent terminality is a durable Session fact and absorbs every
        // derived child lifecycle. Keeping this at the end of the sole warm/cold
        // projector preserves child output/lifecycle ordering, closes each child
        // stream, then closes only the aggregate primary stream.
        self.append_parent_terminal_projection(record);
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }
}
