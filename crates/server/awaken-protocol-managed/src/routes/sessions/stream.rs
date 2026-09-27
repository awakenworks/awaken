//! Managed SSE delivery over the one committed projection and live preview port.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SseTerminalScope {
    /// Session and primary-Thread streams close only at the aggregate boundary.
    Session,
    /// A child-Thread stream closes at that child's idle/terminated boundary.
    ChildThread,
}

/// The terminal committed events that close this exact SSE projection. A child
/// idle is observable on the primary stream but is never the Session terminal.
fn is_terminal(frame: &StreamFrame, scope: SseTerminalScope) -> bool {
    matches!(
        frame,
        StreamFrame::Committed(e)
            if matches!(
                e.type_str(),
                "session.status_idle"
                    | "session.status_terminated"
                    | "session.deleted"
            )
                || (scope == SseTerminalScope::ChildThread
                    && matches!(
                        e.type_str(),
                        "session.thread_status_idle" | "session.thread_status_terminated"
                    ))
    )
}

/// Track whether the newest Run represented in a replay snapshot has reached a
/// terminal event. Usage and telemetry are committed after `status_idle`, so
/// simply asking whether the final snapshot event is terminal leaves a
/// send-then-stream client tailing forever. Conversely, an older idle must not
/// close a stream after a newer input or running marker has started another Run.
pub(super) fn replay_is_terminal_after(
    current: bool,
    event_type: &str,
    scope: SseTerminalScope,
) -> bool {
    match event_type {
        "session.status_idle" | "session.status_terminated" | "session.deleted" => true,
        "session.thread_status_idle" | "session.thread_status_terminated"
            if scope == SseTerminalScope::ChildThread =>
        {
            true
        }
        "session.thread_status_running" | "session.thread_status_rescheduled"
            if scope == SseTerminalScope::ChildThread =>
        {
            false
        }
        "session.thread_status_idle"
        | "session.thread_status_terminated"
        | "session.thread_status_running"
        | "session.thread_status_rescheduled" => current,
        "user.message"
        | "user.tool_confirmation"
        | "user.custom_tool_result"
        | "user.tool_result"
        | "user.define_outcome"
        | "user.interrupt"
        | "session.status_running"
        | "session.status_rescheduled" => false,
        _ => current,
    }
}

fn sse_frame(frame: &StreamFrame) -> SseEvent {
    // The SDK dispatches on the SSE `event:` name; the JSON body carries the same
    // `type` plus the fields (committed event, or a stream-only preview).
    SseEvent::default()
        .event(frame.type_str())
        .data(frame.data())
}

fn accept_preview_frame(
    frame: &crate::types::PreviewFrame,
    selection: PreviewSelection,
    accepted_ids: &mut HashSet<String>,
) -> bool {
    match frame {
        crate::types::PreviewFrame::EventStart { event } => {
            selection.accepts(&event.event_type) && accepted_ids.insert(event.id.clone())
        }
        crate::types::PreviewFrame::EventDelta { event_id, .. } => accepted_ids.contains(event_id),
    }
}

/// One connection observation over the existing committed projection. Its
/// cursor indexes published changes; it is neither durable truth nor a lease.
pub(super) struct LiveSseSource {
    pub(super) state: Arc<ManagedState>,
    pub(super) session_id: String,
    pub(super) revision: u64,
    pub(super) current: bool,
}

/// Snapshot, broadcast wake, silent-period reconciliation and lag recovery all
/// read the same published Event index. Previews remain connection-local.
pub(super) fn live_sse_stream<F>(
    source: LiveSseSource,
    snapshot: Vec<Event>,
    mut rx: broadcast::Receiver<Event>,
    previews: PreviewSelection,
    terminal_scope: SseTerminalScope,
    project: F,
    thread_preview: Option<(
        Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>,
        ThreadPreviewProjector,
    )>,
) -> impl Stream<Item = Result<SseEvent, StateError>>
where
    F: Fn(Event) -> Option<Event> + Send + Sync + 'static,
{
    async_stream::stream! {
        let mut delivery = CommittedEventDelivery::default();
        let mut preview_event_ids = HashSet::new();
        let mut thread_preview = thread_preview;
        let mut terminal_hint = false;
        let mut current = source.current;
        let mut cursor = (source.revision, usize::MAX);
        for event in snapshot {
            let Some(event) = project(event) else { continue; };
            terminal_hint = replay_is_terminal_after(terminal_hint, event.type_str(), terminal_scope);
            match delivery.accept(&event) {
                Ok(true) => yield Ok(sse_frame(&StreamFrame::Committed(event))),
                Ok(false) => {},
                Err(error) => { yield Err(error); return; },
            }
        }
        let mut observation = tokio::time::interval(std::time::Duration::from_secs(1));
        observation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        observation.tick().await;
        let mut tail_due = true;
        let mut refresh_due = !current;
        loop {
            if refresh_due {
                refresh_due = false;
                current = match source.state
                    .refresh_committed_events_for_observation(&source.session_id, None).await
                {
                    Ok(ProjectionObservationDecision::UseRefreshed) => true,
                    Ok(_) => false,
                    Err(_) => {
                        tracing::debug!(session_id = %source.session_id,
                            "Managed stream awaits a coherent committed projection");
                        false
                    }
                };
                tail_due = true;
            }
            if tail_due {
                let page = match source.state.stream_delivery_page(&source.session_id, cursor) {
                    Ok(page) => page,
                    Err(error) => { yield Err(error); return; },
                };
                tail_due = page.len() == ManagedState::STREAM_DELIVERY_PAGE_SIZE;
                for (position, event) in page {
                    cursor = position;
                    let Some(event) = project(event) else { continue; };
                    match delivery.accept(&event) {
                        Ok(false) => continue,
                        Err(error) => { yield Err(error); return; },
                        Ok(true) => {},
                    }
                    if let Some((_, projector)) = thread_preview.as_mut() {
                        if event.type_str() == "agent.message" {
                            for preview in projector.take_for_committed(&event.id) {
                                if accept_preview_frame(&preview, previews, &mut preview_event_ids) {
                                    yield Ok(sse_frame(&StreamFrame::Preview(preview)));
                                }
                            }
                        } else if matches!(event.type_str(), "agent.thread_message_sent"
                            | "session.thread_status_idle" | "session.thread_status_terminated")
                        {
                            projector.discard_uncommitted();
                        }
                    }
                    terminal_hint |= is_terminal(&StreamFrame::Committed(event.clone()), terminal_scope);
                    yield Ok(sse_frame(&StreamFrame::Committed(event)));
                }
                if tail_due { continue; }
            }
            if terminal_hint && current {
                // An earlier complete observation cannot close a newer
                // activity accepted through another replica. Reobserve the
                // original source before terminal admission, then drain any
                // newly published changes before folding its canonical order.
                current = matches!(source.state
                    .refresh_committed_events_for_observation(&source.session_id, None).await,
                    Ok(ProjectionObservationDecision::UseRefreshed));
                if !current { continue; }
                match source.state.stream_delivery_page(&source.session_id, cursor) {
                    Ok(page) if !page.is_empty() => {
                        tail_due = true;
                        continue;
                    }
                    Ok(_) => {},
                    Err(error) => { yield Err(error); return; },
                }
                // A completion enrichment of an old inbound id is not a new
                // activity. Fold canonical order, rather than delivery order,
                // before deciding whether this scope actually finished.
                let canonical = match source.state.stream_projection_subscribe(&source.session_id) {
                    Ok((events, _, _)) => events,
                    Err(error) => { yield Err(error); return; },
                };
                let terminal = canonical.into_iter().filter_map(&project).fold(false,
                    |terminal, event| replay_is_terminal_after(terminal, event.type_str(), terminal_scope));
                if terminal { break; }
                terminal_hint = false;
            }
            tokio::select! {
                biased;
                _ = observation.tick() => { refresh_due = true; }
                received = rx.recv() => match received {
                    Ok(event) if matches!(event.kind, crate::types::OutboundKind::SessionDeleted {}) => {
                        // This producer has already committed the deletion. Its
                        // root is gone and cannot be reconstructed as history.
                        match delivery.accept(&event) {
                            Ok(true) => yield Ok(sse_frame(&StreamFrame::Committed(event))),
                            Ok(false) => {},
                            Err(error) => { yield Err(error); return; },
                        }
                        break;
                    }
                    Ok(_) => { tail_due = true; }
                    Err(broadcast::error::RecvError::Lagged(_)) => { refresh_due = true; }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                live = async {
                    match thread_preview.as_mut() {
                        Some((subscription, _)) => subscription.recv().await,
                        None => std::future::pending().await,
                    }
                } => match live {
                    Ok(Some(event)) => {
                        let frames = thread_preview.as_mut().expect("live subscription exists")
                            .1.project(event);
                        for frame in frames {
                            if accept_preview_frame(&frame, previews, &mut preview_event_ids) {
                                yield Ok(sse_frame(&StreamFrame::Preview(frame)));
                            }
                        }
                    }
                    Ok(None) | Err(_) => thread_preview = None,
                },
            }
        }
    }
}
