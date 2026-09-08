//! The sole warm/cold projection from committed Session, Thread, Run, and Outcome truth.

use super::*;

mod activation_failure;
mod canonical_order;
mod historical_pending;
mod interval_aggregate_projection;
mod publication;
mod refresh_entrypoint;
mod run_observation_projection;
mod usage_projection;
use canonical_order::*;
#[cfg(test)]
pub(super) use canonical_order::{
    budget_reach_close_cursor, budget_reach_projection_close_cursor,
    message_projection_ordinal_bound,
};
use historical_pending::historical_pending_by_lifecycle;

struct CanonicalizationEvidence<'a> {
    persisted: &'a awaken_session_contract::PersistedSession,
    lifecycle_events: &'a [RunLifecycleEvent],
    root_snapshot: Option<&'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    outcome_projections: &'a [AnchoredOutcomeProjection],
    delegation: DelegationProjectionEvidence<'a>,
    root_pending: Option<&'a Pending>,
    child_pending: &'a std::collections::HashMap<String, Pending>,
}

impl ManagedState {
    fn append_root_terminal_error(
        record: &mut SessionRecord,
        terminal: &RunLifecycleEvent,
        outcome_owns_failure: bool,
        snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<(), StateError> {
        if outcome_owns_failure {
            return Ok(());
        }
        let Some(error) = SessionError::from_committed_lifecycle(terminal, snapshot)
            .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?
        else {
            return Ok(());
        };
        let id = managed_multiagent_event_id(
            &record.session.id,
            &record.session.id,
            "root-lifecycle-error",
            ManagedMultiagentEventProvenance::Lifecycle {
                cursor: terminal.cursor.0,
            },
        );
        if !record.events.iter().any(|event| event.id == id) {
            record.events.push(Event {
                id,
                kind: OutboundKind::SessionError { error },
                processed_at: None,
            });
        }
        Ok(())
    }

    async fn refresh_committed_projection_once(
        &self,
        session_id: &str,
    ) -> Result<bool, StateError> {
        // Private Worker realization mutates the same durable Session application
        // without passing through this protocol adapter. Include that root in the
        // same private candidate as Runtime facts so GET cannot publish a stale
        // preparing/idle status or a torn cross-source projection.
        let persisted = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        let persisted_status = Self::wire_session_status(persisted.execution);
        // Snapshot the complete warm projection and its process-local CAS token
        // before asynchronous source reads. Every path below builds privately
        // and publishes through the same compare-and-swap owner.
        let base_record = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or(StateError::NotFound)?;
        let base_cache_revision = base_record.cache_revision;
        let price_snapshot = persisted.budget.price_snapshot().cloned();
        let budget_reach_projections = persisted
            .budget
            .reach_transitions()
            .iter()
            .filter(|transition| budget_reach_close_cursor(&persisted, transition).is_some())
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
        let initial_cursor = base_record.checkpoint.source.lifecycle_cursor;
        // Read Outcome observation before the root recovery fence. A terminal
        // Outcome projection is derived from that same Thread aggregate and its
        // evaluation/state command is the immutable public-order anchor. Reading
        // the fence first could pair an older state prefix with a newer terminal
        // and manufacture an unanchored public event during a concurrent commit.
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
        // Read the root fence before the later relationship snapshot. This
        // ordering makes an unknown lifecycle event classifiable without
        // guessing from its Thread id: a relationship read taken after the
        // root fence must include every coordinated link committed at or before
        // that fence. An event beyond the fence remains unconsumed until the
        // next refresh, when its matching root/link snapshot can catch up.
        let root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        let durable_outcome_projections = durable_outcome_projections
            .into_iter()
            .filter_map(|projection| projection.anchor(root_snapshot.as_ref()))
            .collect::<Vec<_>>();
        let (links, child_snapshots) = self.coordinated_projection_prefix(session_id).await?;
        // The lifecycle feed may advance after the snapshots. Each event carries
        // its source commit cursor, so the recovery snapshot's store cursor can
        // fence it to the exact prefix whose transcript, ticket and disposition
        // were read atomically. A terminal can therefore never close SSE before
        // that same prefix exposes its final output; opaque lifecycle event
        // cursors are never compared with commit-sequence cursors.
        // The disposable cursor is only a live-delivery optimization. Canonical
        // list reconstruction always folds the complete retained lifecycle so a
        // cold replica and a warm replica derive the same interval buckets.
        let lifecycle_events = self.committed_lifecycle_prefix(session_id).await?;
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
            .map(|snapshot| snapshot.messages.as_slice())
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
            Self::assistant_response_coordinates(messages, &root_run_ids);
        let durable_inbound_projections =
            durable_inbound_projections(session_id, &persisted.event_batches);
        let usage = self
            .application
            .session_usage(session_id)
            .await
            .map_err(StateError::Run)?;
        let primary_thread_usage = usage_projection::project_committed_thread_usage(
            root_snapshot.as_ref(),
            price_snapshot.as_ref(),
        )?;
        let mut child_thread_usage = std::collections::HashMap::new();
        for link in &links {
            let usage = usage_projection::project_committed_thread_usage(
                child_snapshots.get(&link.thread_id.0),
                price_snapshot.as_ref(),
            )?;
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
        let all_accepted_lifecycle = accepted_lifecycle.clone();
        let historical_pending =
            historical_pending_by_lifecycle(&all_accepted_lifecycle, |thread_id| {
                if thread_id == session_id {
                    root_snapshot.as_ref()
                } else {
                    child_snapshots.get(thread_id)
                }
            })?;
        for thread_id in &blocked_transcript_threads {
            transcripts.remove(thread_id);
        }

        // Session revision does not change for Runtime commits. Fence the root
        // Thread prefix again after child/link reads, while ignoring the
        // backend-wide diagnostic cursor advanced by unrelated Threads.
        {
            // Validation owns no published payload. Release these second full
            // snapshots before constructing the candidate's wire events.
            let (final_links, final_child_snapshots) =
                self.coordinated_projection_prefix(session_id).await?;
            if links != final_links
                || child_snapshots.len() != final_child_snapshots.len()
                || child_snapshots.iter().any(|(thread_id, initial)| {
                    !same_thread_projection_prefix(
                        Some(initial),
                        final_child_snapshots.get(thread_id),
                    )
                })
            {
                return Ok(false);
            }
            let final_root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
            if !same_thread_projection_prefix(root_snapshot.as_ref(), final_root_snapshot.as_ref())
            {
                return Ok(false);
            }

            let final_persisted = self
                .application
                .session(session_id)
                .await
                .map_err(StateError::from)?;
            if final_persisted.session_id != persisted.session_id
                || final_persisted.revision != persisted.revision
            {
                return Ok(false);
            }
        }

        // Build the complete disposable projection on a private result snapshot. A
        // transcript can be observed between its ToolUse and ToolResult commits,
        // and any later validation/canonicalization error must not retain the
        // earlier mutations (message-consumption marks, cursors, or Events) in
        // this process. Durable Session/Runtime facts remain the sole recovery
        // authority; only a fully valid projection replaces the warm cache.
        let mut staged_record = base_record;
        Self::apply_persisted_session(&mut staged_record, &persisted);
        let previous_event_ids = staged_record
            .events
            .iter()
            .map(|event| event.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let record = &mut staged_record;
        merge_durable_inbound_projections(record, durable_inbound_projections);
        record.primary_thread_usage = primary_thread_usage;
        // Cause C1: an Awaiting interval and its answerable target committed;
        // C2: a later resume/cancel terminal committed before Managed observed
        // that interval; C3: no ToolUse message carries the target. Effect E1:
        // project the same qualified tool event before lifecycle idle reasons;
        // a committed message or prior event remains the canonical projection.
        // Rules: H1=C1+C2+C3=>E1; H2=message/prior event=>reuse unchanged.
        self.append_synthetic_historical_pending_projections(
            record,
            session_id,
            &all_accepted_lifecycle,
            &historical_pending,
            messages,
            &transcripts,
        )?;
        let projected_budget_reach = budget_reach_projections
            .iter()
            .filter(|(generation, _, _)| {
                *generation > record.checkpoint.source.budget_reach_generation
            })
            .cloned()
            .collect::<Vec<_>>();
        let current_cursor = record.checkpoint.source.lifecycle_cursor;
        accepted_lifecycle.retain(|event| event.cursor > current_cursor);
        if accepted_cursor > record.checkpoint.source.lifecycle_cursor {
            record.checkpoint.source.lifecycle_cursor = accepted_cursor;
        }
        let root_pending = root_pending.as_ref();
        let transcript_evidence = project::committed_transcript_evidence(messages, root_pending);
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
        let projected_root_terminal = root_terminal.filter(|event| {
            !record
                .checkpoint
                .progress
                .terminal_cursors
                .contains(&event.cursor)
        });
        // The public aggregate opening belongs to the exact committed root
        // opening transition, not to this Coordinator's process-local
        // observation order. A warm replica can see Running before a cold peer
        // first sees the same prefix at terminal; a resumed Run can also open a
        // later interval under the same Run id. The lifecycle cursor is the one
        // durable coordinate that is both known at the opening and unique for
        // every reopen.
        let is_root_opening = |event: &&RunLifecycleEvent| {
            event.thread_id.0 == session_id
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Running
                        | RunLifecycleEventKind::Resumed
                        | RunLifecycleEventKind::Rescheduled
                )
        };
        let latest_interval_opening = persisted
            .running_interval
            .as_ref()
            .and_then(|interval| {
                let prior_close = persisted
                    .closed_runtime_intervals
                    .iter()
                    .filter_map(interval_close_cursor)
                    .max()
                    .unwrap_or_default();
                running_interval_lifecycle_open_event(
                    &record.session.id,
                    interval,
                    prior_close,
                    &persisted.event_batches,
                    &all_accepted_lifecycle,
                )
            })
            .or_else(|| {
                let (latest, preceding) = persisted.closed_runtime_intervals.split_last()?;
                let prior_close = preceding
                    .iter()
                    .filter_map(interval_close_cursor)
                    .max()
                    .unwrap_or_default();
                interval_lifecycle_open_event(
                    &record.session.id,
                    latest,
                    prior_close,
                    &persisted.event_batches,
                    &all_accepted_lifecycle,
                )
            });
        let aggregate_running_event_id = latest_interval_opening
            .or_else(|| {
                root_terminal.and_then(|terminal| {
                    // The opening may have been consumed by an earlier refresh
                    // while Session activity was not yet Running. A later terminal
                    // must still recover that exact bracket from the complete
                    // accepted prefix. Select the latest opening of this same Run:
                    // one Run can Await and Resume more than once.
                    all_accepted_lifecycle
                        .iter()
                        .rev()
                        .filter(is_root_opening)
                        .find(|event| event.run_id == terminal.run_id)
                })
            })
            .or_else(|| accepted_lifecycle.iter().rev().find(is_root_opening))
            .map(|event| {
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "aggregate-status-running",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: event.cursor.0,
                    },
                )
            })
            .or_else(|| {
                // A fast Run can be first observed only at its terminal
                // boundary. In that case there is no retained opening cursor,
                // but the terminal prefix already reconstructs the primary
                // Thread Running edge. Use the same immutable terminal cursor
                // for a distinct aggregate fallback identity; an exact opening
                // always wins when one is available.
                root_terminal.map(|terminal| {
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-terminal-status-running",
                        ManagedMultiagentEventProvenance::LifecyclePrefix {
                            cursor: terminal.cursor.0,
                        },
                    )
                })
            });
        let aggregate_running_transition =
            Self::should_append_aggregate_running(record, persisted_status);
        if aggregate_running_transition && let Some(id) = aggregate_running_event_id.clone() {
            // A recovery refresh may see the same opening while the aggregate
            // status and lifecycle prefix catch up independently. Reuse the one
            // interval projection identity owner; repeated reads are not events.
            Self::append_aggregate_running_once(record, id);
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
        if projected_root_terminal.is_some()
            && persisted_status == SessionStatus::Idle
            && !aggregate_running_transition
            && Self::needs_aggregate_running_edge(record)
            && let Some(id) = aggregate_running_event_id
        {
            // A recovery refresh may see the same opening while the aggregate
            // status and lifecycle prefix catch up independently. Reuse the one
            // interval projection identity owner; repeated reads are not events.
            Self::append_aggregate_running_once(record, id);
        }
        // Root Thread lifecycle is projected from the same accepted lifecycle
        // prefix as the aggregate Session edges. A foreground command already
        // inserted the bracket when its terminal cursor is present in the one
        // terminal set; otherwise warm/cold recovery emits every newly accepted
        // Running/Rescheduled edge. If a cold prefix starts at a terminal, the
        // event tail reconstructs the missing opening edge before output.
        let project_root_transitions = root_terminal.is_none() || projected_root_terminal.is_some();
        let mut projected_root_opening = false;
        if project_root_transitions {
            for lifecycle in accepted_lifecycle.iter().filter(|event| {
                event.thread_id.0 == session_id
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Running
                            | RunLifecycleEventKind::Resumed
                            | RunLifecycleEventKind::Rescheduled
                    )
            }) {
                match lifecycle.kind {
                    RunLifecycleEventKind::Running | RunLifecycleEventKind::Resumed => {
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
                            processed_at: None,
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
            && let Some(terminal) = projected_root_terminal
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
            let historical_boundaries = Self::historical_message_boundaries(
                message,
                &historical_pending,
                &all_accepted_lifecycle,
                session_id,
                root_response_coordinates
                    .get(&message.id.0)
                    .map(|(run, _, _)| run.as_str()),
            );
            let message_source_run_id = root_pending_source_run_id
                .or_else(|| historical_boundaries.first().map(|(_, run)| *run));
            let tool_index = record.projected_tool_index_for(None);
            let pending_history = historical_boundaries
                .iter()
                .map(|(pending, _)| pending)
                .collect::<Vec<_>>();
            let selected = transcript_evidence.project_message(
                message,
                &tool_index.sources,
                message_source_run_id.map(|run_id| run_id.0.as_str()),
                &pending_history,
            );
            pending_is_in_new_message |= selected.pending_occurrence;
            // The exact current ticket wins over any earlier call in this
            // source message. Historical permissions are applied per call by
            // the shared encoder, never chosen by HashMap iteration order.
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
                &pending_history,
            );
            self.qualify_tool_projection(
                record,
                None,
                std::slice::from_ref(&selected.message),
                message_source_run_id,
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
                    processed_at: None,
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
                &[],
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
                        processed_at: None,
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

        let delegation_evidence = DelegationProjectionEvidence {
            links: &links,
            snapshots: &child_snapshots,
            transcripts: &transcripts,
            lifecycle_events: &accepted_lifecycle,
            latest_run_states: &child_latest_run_states,
            pending: &child_pending,
            historical_pending: &historical_pending,
            dispositions: &child_dispositions,
            usage: &child_thread_usage,
        };
        self.append_delegation_projections(record, delegation_evidence)?;

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
        if let Some(terminal) = projected_root_terminal {
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
        if let Some(terminal) = projected_root_terminal {
            record
                .checkpoint
                .progress
                .terminal_cursors
                .insert(terminal.cursor);
            Self::append_root_terminal_error(
                record,
                terminal,
                durable_outcome_projections
                    .iter()
                    .any(|projection| projection.owns_failure_run(&terminal.run_id)),
                root_snapshot.as_ref(),
            )?;
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
            && (projected_root_terminal.is_some()
                || child_terminal.is_some()
                || record.deferred_session_stop_reason.is_some()
                || (!projected_budget_reach.is_empty() && pending_event_ids.is_empty()))
        {
            if pending_event_ids.is_empty() && !projected_budget_reach.is_empty() {
                for (generation, usage, budget) in projected_budget_reach {
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
                            processed_at: None,
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
                            processed_at: None,
                        },
                    ]);
                    record.checkpoint.source.budget_reach_generation = generation;
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
                // The durable BudgetReachTransition is the sole aggregate
                // budget-pause authority. A lifecycle Awaiting fact may become
                // visible one refresh before that transition, or after its
                // generation was already projected; publishing it here would
                // create a second `session.status_idle:budget_reached` for one
                // cap transition. Thread lifecycle remains projected above.
                if !matches!(stop_reason, StopReason::BudgetReached) {
                    // Only a terminal lifecycle prefix not yet represented by an
                    // aggregate idle is replayable here. The shared terminal-cursor
                    // set covers root and children.
                    let aggregate_terminal_cursor = record
                        .checkpoint
                        .progress
                        .terminal_cursors
                        .iter()
                        .max()
                        .copied()
                        .ok_or_else(|| {
                            StateError::Run(RunError::internal(
                                "idle aggregate has no committed terminal lifecycle boundary",
                            ))
                        })?;
                    let aggregate_usage_id = managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-usage",
                        ManagedMultiagentEventProvenance::LifecyclePrefix {
                            cursor: aggregate_terminal_cursor.0,
                        },
                    );
                    let aggregate_idle_id = managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        "aggregate-status-idle",
                        ManagedMultiagentEventProvenance::LifecyclePrefix {
                            cursor: aggregate_terminal_cursor.0,
                        },
                    );
                    record.events.extend([
                        Event {
                            id: aggregate_usage_id,
                            kind: OutboundKind::SessionUsage {
                                usage: projected_usage,
                                budget: record.session.budget.clone(),
                            },
                            processed_at: None,
                        },
                        Event {
                            id: aggregate_idle_id,
                            kind: OutboundKind::SessionStatusIdle { stop_reason },
                            processed_at: None,
                        },
                    ]);
                }
            }
            record.deferred_session_stop_reason = None;
        }
        // Parent terminality is a durable Session fact and absorbs every
        // derived child lifecycle. Keeping this at the end of the sole warm/cold
        // projector preserves child output/lifecycle ordering, closes each child
        // stream, then closes only the aggregate primary stream.
        self.append_activation_failure_error_projection(
            record,
            &persisted,
            &all_accepted_lifecycle,
        )?;
        if persisted.execution == awaken_session_contract::SessionExecutionState::ActivationFailed
            || persisted.terminal_cleanup.runtime_commit_cursor().is_some()
        {
            self.append_parent_terminal_projection(record);
        }
        // A cold prefix can contain multiple completed root turns. The legacy
        // projector lowered only its latest terminal, so repair every retained
        // root boundary by the same lifecycle identity before interval ordering.
        for terminal in all_accepted_lifecycle.iter().filter(|event| {
            event.thread_id.0 == session_id
                && matches!(
                    event.kind,
                    RunLifecycleEventKind::Awaiting
                        | RunLifecycleEventKind::Completed
                        | RunLifecycleEventKind::Failed
                        | RunLifecycleEventKind::Cancelled
                )
        }) {
            let idle_id = managed_multiagent_event_id(
                &record.session.id,
                &record.session.id,
                "root-lifecycle-status-idle",
                ManagedMultiagentEventProvenance::Lifecycle {
                    cursor: terminal.cursor.0,
                },
            );
            if record.events.iter().any(|event| event.id == idle_id) {
                continue;
            }
            Self::append_root_terminal_error(
                record,
                terminal,
                durable_outcome_projections
                    .iter()
                    .any(|projection| projection.owns_failure_run(&terminal.run_id)),
                root_snapshot.as_ref(),
            )?;
            let stop_reason = Self::public_run_stop_reason(
                record,
                None,
                &terminal.state,
                historical_pending
                    .get(&terminal.cursor)
                    .or(root_pending.filter(|_| root_terminal == Some(terminal))),
                terminal.await_reason.as_ref(),
            )?;
            record.events.push(primary_thread_status_event(
                record,
                idle_id,
                PrimaryThreadStatusProjection::Idle { stop_reason },
            ));
            record
                .checkpoint
                .progress
                .terminal_cursors
                .insert(terminal.cursor);
        }
        Self::replace_interval_aggregate_projections(
            record,
            &persisted,
            &all_accepted_lifecycle,
            &historical_pending,
            price_snapshot.as_ref(),
        )?;
        Self::canonicalize_committed_events(
            record,
            CanonicalizationEvidence {
                persisted: &persisted,
                lifecycle_events: &all_accepted_lifecycle,
                root_snapshot: root_snapshot.as_ref(),
                outcome_projections: &durable_outcome_projections,
                delegation: delegation_evidence,
                root_pending,
                child_pending: &child_pending,
            },
        )?;
        publication::stamp_source(
            &mut staged_record,
            persisted.revision,
            root_snapshot.as_ref(),
            &child_snapshots,
        );

        self.publish_projection_candidate(
            session_id,
            base_cache_revision,
            staged_record,
            &previous_event_ids,
        )
    }
}
