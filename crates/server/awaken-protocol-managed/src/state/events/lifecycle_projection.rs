//! Child lifecycle, disposition, delegation, and aggregate terminal projection.

use super::*;

impl ManagedState {
    pub(super) fn run_stop_reason(
        state: &awaken_agent_contract::agent::run::RunState,
        pending: Option<&Pending>,
        await_reason: Option<&awaken_agent_contract::agent::awaiting::AwaitReason>,
    ) -> StopReason {
        project::terminal_stop_reason(state, pending, await_reason, false)
    }

    pub(super) fn public_run_stop_reason(
        record: &SessionRecord,
        owner_thread_id: Option<&str>,
        state: &awaken_agent_contract::agent::run::RunState,
        pending: Option<&Pending>,
        await_reason: Option<&awaken_agent_contract::agent::awaiting::AwaitReason>,
    ) -> Result<StopReason, StateError> {
        let mut reason = Self::run_stop_reason(state, pending, await_reason);
        if let StopReason::RequiresAction { event_ids } = &mut reason {
            let latest = record.projected_tool_index_for(owner_thread_id).latest;
            for event_id in event_ids {
                *event_id = latest.get(event_id).cloned().ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "Managed pending ticket has no answerable public tool event",
                    ))
                })?;
            }
        }
        Ok(reason)
    }

    pub(super) fn child_stop_reason(
        event: &RunLifecycleEvent,
        pending: Option<&Pending>,
    ) -> StopReason {
        Self::run_stop_reason(&event.state, pending, event.await_reason.as_ref())
    }

    pub(super) fn append_child_lifecycle_projection(
        &self,
        record: &mut SessionRecord,
        events: &[RunLifecycleEvent],
        pending: &std::collections::HashMap<String, Pending>,
    ) -> Result<(), StateError> {
        let latest_cursor_by_thread =
            events
                .iter()
                .fold(std::collections::HashMap::new(), |mut latest, event| {
                    latest.insert(event.thread_id.0.as_str(), event.cursor);
                    latest
                });
        for event in events {
            let thread_id = event.thread_id.0.as_str();
            let Some(index) = record
                .child_threads
                .iter()
                .position(|child| child.id == thread_id)
            else {
                continue;
            };
            if record.child_threads[index].status == SessionThreadStatus::Terminated {
                continue;
            }
            let agent_name = record.child_threads[index].agent.display_name().to_string();
            let is_advisor = record.child_threads[index].agent.is_advisor();
            let ordinary_failed = !is_advisor && event.kind == RunLifecycleEventKind::Failed;
            let advisor_self_terminal = is_advisor
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                );
            match event.kind {
                RunLifecycleEventKind::Running | RunLifecycleEventKind::Resumed => {
                    if record.child_threads[index].status != SessionThreadStatus::Running {
                        record.child_threads[index].status = SessionThreadStatus::Running;
                        record.child_threads[index].updated_at = PROCESSED_AT.to_string();
                        record.events.push(Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                thread_id,
                                "lifecycle-status-running",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: event.cursor.0,
                                },
                            ),
                            kind: OutboundKind::SessionThreadStatusRunning {
                                session_thread_id: thread_id.to_string(),
                                agent_name,
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        });
                    }
                }
                RunLifecycleEventKind::Rescheduled => {
                    // One committed retry receipt represents both observable
                    // edges: the old attempt was rescheduled, then its already
                    // claimed replacement began running. Keeping both under the
                    // same lifecycle cursor makes warm replay and cold rebuild
                    // converge without a protocol-owned retry registry.
                    record.child_threads[index].status = SessionThreadStatus::Rescheduling;
                    record.child_threads[index].updated_at = PROCESSED_AT.to_string();
                    record.events.push(Event {
                        id: managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            "lifecycle-status-rescheduled",
                            ManagedMultiagentEventProvenance::Lifecycle {
                                cursor: event.cursor.0,
                            },
                        ),
                        kind: OutboundKind::SessionThreadStatusRescheduled {
                            session_thread_id: thread_id.to_string(),
                            agent_name: agent_name.clone(),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                    record.child_threads[index].status = SessionThreadStatus::Running;
                    record.events.push(Event {
                        id: managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            "rescheduled-lifecycle-status-running",
                            ManagedMultiagentEventProvenance::Lifecycle {
                                cursor: event.cursor.0,
                            },
                        ),
                        kind: OutboundKind::SessionThreadStatusRunning {
                            session_thread_id: thread_id.to_string(),
                            agent_name,
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
                RunLifecycleEventKind::Awaiting
                | RunLifecycleEventKind::Completed
                | RunLifecycleEventKind::Failed
                | RunLifecycleEventKind::Cancelled => {
                    let terminal_pending = (latest_cursor_by_thread.get(thread_id)
                        == Some(&event.cursor))
                    .then(|| pending.get(thread_id))
                    .flatten();
                    let budget_reached = event.await_reason.as_ref()
                        == Some(
                            &awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached,
                        );
                    if event.kind == RunLifecycleEventKind::Awaiting
                        && terminal_pending.is_none()
                        && !budget_reached
                    {
                        // A historical Awaiting fact whose ticket has already
                        // been consumed cannot be reconstructed as an answerable
                        // Managed idle. Emitting `requires_action([])` would be a
                        // false public state; the later Resumed/terminal facts
                        // still project from the same lifecycle feed.
                        continue;
                    }
                    let stop_reason = Self::public_run_stop_reason(
                        record,
                        Some(thread_id),
                        &event.state,
                        terminal_pending,
                        event.await_reason.as_ref(),
                    )?;
                    if let awaken_agent_contract::agent::run::RunState::Ended(
                        awaken_agent_contract::agent::run::EndCause::Error(failure),
                    ) = &event.state
                    {
                        let id = managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            "lifecycle-error",
                            ManagedMultiagentEventProvenance::Lifecycle {
                                cursor: event.cursor.0,
                            },
                        );
                        record
                            .event_thread_owners
                            .insert(id.clone(), thread_id.to_string());
                        record.events.push(Event {
                            id,
                            kind: OutboundKind::SessionError {
                                error: if is_advisor {
                                    SessionError::classify(
                                        "advisor_consultation_failed",
                                        ADVISOR_FAILURE_NOTICE,
                                    )
                                } else {
                                    SessionError::classify(failure.code(), failure.message())
                                },
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        });
                    }
                    record.projected_terminal_cursors.insert(event.cursor);
                    if ordinary_failed {
                        // A committed Failed Run is the absorbing admission
                        // fence for an ordinary coordinated Thread. Project the
                        // error first and terminate directly: publishing Idle
                        // would promise follow-up input that Runtime must reject.
                        record.child_threads[index].status = SessionThreadStatus::Terminated;
                        record.child_threads[index].updated_at = PROCESSED_AT.to_string();
                        record.events.push(Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                thread_id,
                                "failed-lifecycle-status-terminated",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: event.cursor.0,
                                },
                            ),
                            kind: OutboundKind::SessionThreadStatusTerminated {
                                session_thread_id: thread_id.to_string(),
                                agent_name,
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        });
                        continue;
                    }
                    record.child_threads[index].status = SessionThreadStatus::Idle;
                    record.child_threads[index].updated_at = PROCESSED_AT.to_string();
                    record.events.push(Event {
                        id: managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            "lifecycle-status-idle",
                            ManagedMultiagentEventProvenance::Lifecycle {
                                cursor: event.cursor.0,
                            },
                        ),
                        kind: OutboundKind::SessionThreadStatusIdle {
                            session_thread_id: thread_id.to_string(),
                            agent_name: agent_name.clone(),
                            stop_reason,
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                    if advisor_self_terminal {
                        // Each consultation owns one Thread and self-terminates
                        // after its one ordinary Run. This terminal is derived
                        // from the durable advisor target + Run boundary, not a
                        // generic child archive cache.
                        record.child_threads[index].status = SessionThreadStatus::Terminated;
                        record.child_threads[index].archived_at = Some(PROCESSED_AT.to_string());
                        record.events.push(Event {
                            id: managed_multiagent_event_id(
                                &record.session.id,
                                thread_id,
                                "advisor-lifecycle-status-terminated",
                                ManagedMultiagentEventProvenance::Lifecycle {
                                    cursor: event.cursor.0,
                                },
                            ),
                            kind: OutboundKind::SessionThreadStatusTerminated {
                                session_thread_id: thread_id.to_string(),
                                agent_name,
                            },
                            processed_at: Some(PROCESSED_AT.to_string()),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn append_child_disposition_projection(
        &self,
        record: &mut SessionRecord,
        dispositions: &std::collections::HashMap<String, awaken_agent_contract::ThreadDisposition>,
    ) {
        for child in &mut record.child_threads {
            if dispositions.get(&child.id)
                != Some(&awaken_agent_contract::ThreadDisposition::Archived)
                || child.status == SessionThreadStatus::Terminated
            {
                continue;
            }
            let agent_name = child.agent.display_name().to_string();
            child.status = SessionThreadStatus::Terminated;
            child.archived_at = Some(PROCESSED_AT.to_string());
            child.updated_at = PROCESSED_AT.to_string();
            record.events.push(Event {
                id: managed_multiagent_event_id(
                    &record.session.id,
                    &child.id,
                    "archived-status-terminated",
                    ManagedMultiagentEventProvenance::ArchivedDisposition,
                ),
                kind: OutboundKind::SessionThreadStatusTerminated {
                    session_thread_id: child.id.clone(),
                    agent_name,
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
    }

    /// Project the absorbing parent Session terminal over every derived Thread.
    /// `PersistedSession.execution` remains the only terminal authority: child
    /// status and events here are disposable wire projections of that fact, not
    /// independently persisted child dispositions. Child terminals precede the
    /// aggregate terminal so each child SSE can close while the primary stream
    /// continues until `session.status_terminated`.
    pub(super) fn append_parent_terminal_projection(&self, record: &mut SessionRecord) {
        if record.session.status != SessionStatus::Terminated {
            return;
        }
        for child in &mut record.child_threads {
            if child.status == SessionThreadStatus::Terminated {
                continue;
            }
            child.status = SessionThreadStatus::Terminated;
            child.archived_at = record.session.archived_at.clone();
            child.updated_at = record
                .session
                .archived_at
                .clone()
                .unwrap_or_else(|| PROCESSED_AT.to_string());
            record.events.push(Event {
                id: managed_multiagent_event_id(
                    &record.session.id,
                    &child.id,
                    "parent-terminal-child-status-terminated",
                    ManagedMultiagentEventProvenance::ParentTerminal,
                ),
                kind: OutboundKind::SessionThreadStatusTerminated {
                    session_thread_id: child.id.clone(),
                    agent_name: child.agent.display_name().to_string(),
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        let primary_terminated_id = managed_multiagent_event_id(
            &record.session.id,
            &record.session.id,
            "parent-terminal-primary-status-terminated",
            ManagedMultiagentEventProvenance::ParentTerminal,
        );
        if !record
            .events
            .iter()
            .any(|event| event.id == primary_terminated_id)
        {
            let primary_terminated = primary_thread_status_event(
                record,
                primary_terminated_id,
                PrimaryThreadStatusProjection::Terminated,
            );
            record.events.push(primary_terminated);
        }
        if record
            .events
            .iter()
            .any(|event| matches!(event.kind, OutboundKind::SessionStatusTerminated { .. }))
        {
            return;
        }
        record.events.push(Event {
            id: managed_multiagent_event_id(
                &record.session.id,
                &record.session.id,
                "parent-terminal-session-status-terminated",
                ManagedMultiagentEventProvenance::ParentTerminal,
            ),
            kind: OutboundKind::SessionStatusTerminated {},
            processed_at: Some(PROCESSED_AT.to_string()),
        });
    }

    /// Decide the aggregate Running edge from committed Session state and the
    /// event projection while the record lock is held. `record.session.status`
    /// is only the current disposable DTO: a foreground command can refresh it
    /// to Running before the matching public edge is projected. The latest
    /// aggregate status event is the projector's existing idempotency cursor and
    /// needs no parallel status flag.
    pub(super) fn should_append_aggregate_running(
        record: &SessionRecord,
        persisted_status: SessionStatus,
    ) -> bool {
        persisted_status == SessionStatus::Running && Self::needs_aggregate_running_edge(record)
    }

    /// Whether the append-only wire history still needs an aggregate Running
    /// opening edge. Both a live Running refresh and a later terminal-prefix
    /// reconstruction use this one tail classification, so observing durable
    /// Running before the terminal commit cannot mint a second opening edge.
    pub(super) fn needs_aggregate_running_edge(record: &SessionRecord) -> bool {
        record
            .events
            .iter()
            .rev()
            .find_map(|event| match event.kind {
                OutboundKind::SessionStatusRunning { .. }
                | OutboundKind::SessionStatusTerminated { .. } => Some(false),
                OutboundKind::SessionStatusIdle { .. }
                | OutboundKind::SessionStatusRescheduled { .. } => Some(true),
                _ => None,
            })
            .unwrap_or(true)
    }

    /// Whether the latest public lifecycle edge for the primary Thread is open.
    /// The committed event tail is the existing projector idempotency authority;
    /// no parallel primary-status cell is maintained.
    pub(super) fn primary_thread_status_is_open(record: &SessionRecord) -> bool {
        let primary_id = public_thread_id(&record.session.id, &record.session.id);
        record
            .events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                OutboundKind::SessionThreadStatusRunning {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(true),
                OutboundKind::SessionThreadStatusRescheduled {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(true),
                OutboundKind::SessionThreadStatusIdle {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(false),
                OutboundKind::SessionThreadStatusTerminated {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Sole Runtime relationship -> Managed child Thread/event projector. Warm
    /// refresh and cold rehydration call this same function. Its inputs are real
    /// child Thread links, parent-partition transcripts, and ordinary Run
    /// lifecycle facts; a synchronous legacy `DelegatedRun.run_id` is never
    /// reinterpreted as a Thread id.
    pub(super) fn append_delegation_projections(
        &self,
        record: &mut SessionRecord,
        evidence: DelegationProjectionEvidence<'_>,
    ) -> Result<(), StateError> {
        let DelegationProjectionEvidence {
            links,
            snapshots,
            transcripts,
            lifecycle_events,
            latest_run_states,
            pending,
            dispositions,
            usage,
        } = evidence;
        let mut seen = std::collections::HashSet::new();
        let resolved = links
            .iter()
            .map(|link| {
                if !seen.insert(link.thread_id.0.clone()) {
                    return Err(StateError::Run(RunError::internal(
                        "Runtime returned a duplicate coordinated Thread link",
                    )));
                }
                Self::coordinated_thread_agent(record, link).map(|agent| (link, agent))
            })
            .collect::<Result<Vec<_>, _>>()?;
        record
            .child_threads
            .retain(|child| seen.contains(&child.id));
        for (link, agent) in resolved {
            let thread_id = link.thread_id.0.clone();
            let name = agent.display_name().to_string();
            let existing = record
                .child_threads
                .iter()
                .position(|thread| thread.id == thread_id);
            let is_new = existing.is_none();
            let index = existing.unwrap_or_else(|| {
                let child = Self::child_thread(&record.session, &thread_id, agent);
                record.child_threads.push(child);
                record.child_threads.len() - 1
            });
            record.child_threads[index].usage = usage.get(&thread_id).cloned().flatten();
            if is_new {
                record.events.extend([
                    Event {
                        id: managed_multiagent_event_id(
                            &record.session.id,
                            &thread_id,
                            "link-thread-created",
                            ManagedMultiagentEventProvenance::LinkOperation {
                                operation_id: &link.created_by_operation_id,
                            },
                        ),
                        kind: OutboundKind::SessionThreadCreated {
                            session_thread_id: thread_id.clone(),
                            agent_name: name.clone(),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                    Event {
                        id: managed_multiagent_event_id(
                            &record.session.id,
                            &thread_id,
                            "link-status-running",
                            ManagedMultiagentEventProvenance::LinkOperation {
                                operation_id: &link.created_by_operation_id,
                            },
                        ),
                        kind: OutboundKind::SessionThreadStatusRunning {
                            session_thread_id: thread_id.clone(),
                            agent_name: name.clone(),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                ]);
            }
            debug_assert_eq!(record.child_threads[index].id, thread_id);
        }
        self.append_coordination_cross_posts(record, links);
        for link in links {
            let thread_id = link.thread_id.0.as_str();
            let child = record
                .child_threads
                .iter()
                .find(|child| child.id == thread_id)
                .expect("validated coordinated Thread is cached");
            let agent_name = child.agent.display_name().to_string();
            let advisor_model = match &child.agent {
                crate::types::SessionThreadAgentValue::Advisor(advisor) => {
                    Some(advisor.model.clone())
                }
                crate::types::SessionThreadAgentValue::Agent(_) => None,
            };
            let child_lifecycle_events = lifecycle_events
                .iter()
                .filter(|event| event.thread_id.0 == thread_id)
                .cloned()
                .collect::<Vec<_>>();
            if transcripts.contains_key(thread_id)
                && let Some(snapshot) = snapshots.get(thread_id)
            {
                Self::append_run_observation_projections(record, thread_id, snapshot)?;
            }
            self.append_child_transcript_projection(
                record,
                ChildTranscriptProjection {
                    thread_id,
                    agent_name: &agent_name,
                    advisor_model: advisor_model.as_deref(),
                    messages: transcripts
                        .get(thread_id)
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                    lifecycle_events: &child_lifecycle_events,
                    latest_run_id: link.latest_run_id.as_ref(),
                    latest_run_state: latest_run_states.get(thread_id),
                    pending: pending.get(thread_id),
                    pending_source_run_id: lifecycle_events
                        .iter()
                        .rev()
                        .find(|event| {
                            event.thread_id.0 == thread_id
                                && event.kind == RunLifecycleEventKind::Awaiting
                        })
                        .map(|event| &event.run_id),
                },
            )?;
        }
        self.append_child_lifecycle_projection(record, lifecycle_events, pending)?;
        // Archive is an absorbing Thread disposition, so it is projected after
        // historical Run lifecycle. On warm refresh it prevents later lifecycle
        // pages from reviving the child; on cold rebuild it closes the same
        // created/running/message/idle history with one terminal event.
        self.append_child_disposition_projection(record, dispositions);
        record
            .projected_child_latest_run_ids
            .retain(|thread_id, _| seen.contains(thread_id));
        for link in links {
            if let Some(latest_run_id) = &link.latest_run_id {
                record
                    .projected_child_latest_run_ids
                    .insert(link.thread_id.0.clone(), latest_run_id.clone());
            }
        }
        Ok(())
    }
}
