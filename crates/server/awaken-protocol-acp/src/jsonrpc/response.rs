// @formal entrypoint acp.boundary_preserves_permission_authority_and_effect_order
//! Incoming ACP response, notification, and agent-request pump.
//!
//! This module is the single owner of interleaving a target JSON-RPC response
//! with streamed Session facts and fail-closed permission requests. Handshake
//! and prompt orchestration remain in the parent driver.

use super::*;

/// Read messages until the response to `target_id` arrives, meanwhile projecting
/// `session/update` notifications into `sink` and answering agent→client requests
/// fail-closed. Returns the matching response's `result`, or an error if the
/// agent answered `target_id` with a JSON-RPC error or the stream ended first.
pub(super) async fn pump_to_response(
    wire: &mut Wire<'_>,
    target_id: u64,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
) -> Result<serde_json::Value, AcpError> {
    pump_response(wire, target_id, sink, seq, resolver)
        .await?
        .into_result()
}

/// Read the active prompt to its business boundary, then settle a suspended
/// turn before the caller tears down its opaque process.
pub(super) async fn pump_prompt(
    wire: &mut Wire<'_>,
    session_id: &SessionId,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
    settle_deadline: std::time::Duration,
) -> Result<serde_json::Value, AcpError> {
    let mut context = PermissionContext::for_session(session_id);
    let outcome = pump_response_in_context(wire, ID_PROMPT, sink, seq, resolver, &mut context)
        .await
        .and_then(RpcResponse::into_result);
    if let Err(AcpError::PermissionAwait { ask, .. }) = &outcome {
        // Cancelling the permission request alone does not finish the SDK turn.
        // Let the adapter interrupt and flush its session before process teardown.
        // The pending call and new work remain cancellation residue, but an
        // already observed sibling may have executed before this Await. Retain
        // its exact late terminal fact through the same sink and sequence.
        context.begin_settlement(&ask.call_id);
        let settle = async {
            wire.send(&serde_json::json!({
                "jsonrpc": JSONRPC,
                "method": AGENT_METHOD_NAMES.session_cancel,
                "params": agent_client_protocol::CancelNotification::new(session_id.clone()),
            }))
            .await?;
            pump_response_in_context(wire, ID_PROMPT, sink, seq, &RejectPermission, &mut context)
                .await
        };
        // EOF, a bad acknowledgement or timeout cannot replace the durable Await
        // with success/failure. The supervisor still owns bounded physical reap.
        let _ = tokio::time::timeout(settle_deadline, settle).await;
    }
    outcome
}

pub(super) enum RpcResponse {
    Result(serde_json::Value),
    Error(serde_json::Value),
}

impl RpcResponse {
    fn into_result(self) -> Result<serde_json::Value, AcpError> {
        match self {
            Self::Result(result) => Ok(result),
            Self::Error(error) => Err(AcpError::Frame(error.to_string())),
        }
    }
}

pub(super) fn is_missing_session_error(error: &serde_json::Value) -> bool {
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    message.contains("unknown session")
        || message.contains("session not found")
        || message.contains("no such session")
}

/// The response pump with a typed JSON-RPC error branch. Most call sites retain
/// fail-closed behavior through [`pump_to_response`]; `session/load` uses the
/// explicit error branch to fall back before any prompt or side effect occurs.
pub(super) async fn pump_response(
    wire: &mut Wire<'_>,
    target_id: u64,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
) -> Result<RpcResponse, AcpError> {
    pump_response_in_context(
        wire,
        target_id,
        sink,
        seq,
        resolver,
        &mut PermissionContext::default(),
    )
    .await
}

async fn pump_response_in_context(
    wire: &mut Wire<'_>,
    target_id: u64,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
    permission_context: &mut PermissionContext,
) -> Result<RpcResponse, AcpError> {
    // Some ACP adapters (notably Codex) report the precise MCP identity only in
    // the preceding tool_call update, then ask permission for a generic
    // `execute` operation. The ACP toolCallId is the protocol correlation key;
    // retain the already-projected identity for the lifetime of this response
    // pump so the one neutral policy sees the real tool rather than a lossy title.
    loop {
        let Some(msg) = wire.read().await? else {
            return Err(AcpError::Truncated);
        };
        match &msg.id {
            // A response or an agent→client request (both carry an id).
            Some(id) if msg.method.is_none() => {
                if id.as_u64() != Some(target_id) {
                    continue; // a stale response to an earlier id
                }
                if let Some(error) = msg.error {
                    return Ok(RpcResponse::Error(error));
                }
                return Ok(RpcResponse::Result(
                    msg.result.unwrap_or(serde_json::Value::Null),
                ));
            }
            Some(request_id) => {
                let method = msg.method.as_deref().unwrap_or_default();
                answer_request(
                    wire,
                    request_id.clone(),
                    method,
                    msg.params,
                    resolver,
                    permission_context,
                    (sink, seq),
                )
                .await?;
            }
            // A notification (no id).
            None => {
                if msg.method.as_deref() == Some(CLIENT_METHOD_NAMES.session_update) {
                    project_notification(msg.params, sink, seq, permission_context).await?;
                }
            }
        }
    }
}

/// Project one `session/update` notification into the sink (skips updates with no
/// runtime projection: user echoes, thoughts, plans, argument-free progress).
async fn project_notification(
    params: Option<serde_json::Value>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    permission_context: &mut PermissionContext,
) -> Result<(), AcpError> {
    let Some(params) = params else {
        return Ok(());
    };
    let notification: SessionNotification = parse(params)?;
    if !permission_context.accepts_session(&notification.session_id) {
        return Ok(());
    }
    if permission_context.is_settling() {
        if let Some(event) = permission_context.settlement_result(&notification.update) {
            *seq += 1;
            sink.append(*seq, &event).await?;
            permission_context.observe(&event);
        }
        return Ok(());
    }
    // One update can finish arguments and report a result together. Refine the
    // attempt-local call first, then project its result through the same sink.
    if let agent_client_protocol::SessionUpdate::ToolCallUpdate(update) = &notification.update
        && let Some(event) = permission_context.refined_call(update)
    {
        *seq += 1;
        sink.append(*seq, &event).await?;
        permission_context.observe(&event);
    }
    if let Some(event) = project_update(&notification.update) {
        // A provider HARD-quota banner can arrive as assistant TEXT ("You've hit
        // your weekly limit · resets …") rather than a structured error — the case
        // where the CLI then hangs. Consult the detector before committing: a
        // recognized banner fails the turn closed with the classified failure
        // (RateLimited → Error) instead of landing as an ordinary assistant
        // message. Any other text still projects normally below.
        if let AcpProjectedEvent::Message { text, .. } = &event
            && let Some(failure) = crate::streamed_hard_limit(text)
        {
            return Err(AcpError::HardLimit(failure));
        }
        *seq += 1;
        sink.append(*seq, &event).await?;
        permission_context.observe(&event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Sink(Vec<AcpProjectedEvent>);
    #[async_trait::async_trait]
    impl RunFactAppender for Sink {
        async fn append(
            &mut self,
            _: u64,
            event: &AcpProjectedEvent,
        ) -> Result<(), crate::AppendError> {
            self.0.push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn permission_await_settles_without_committing_late_work_or_changing_the_ticket() {
        // Causes: durable Await; late permission/notifications; terminal ack,
        // closed wire, malformed frame, or absent ack. R1 ack -> wait for ack;
        // R2 late permission -> reject, do not call original policy; R3/R4/R5
        // EOF/malformed/deadline -> retain original Await, bounded return.
        // All rows: only original ToolUse reaches the sink; no late result/text
        // replaces the pending operation. This is cancellation, not task success.
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        struct Awaiting(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl PermissionResolver for Awaiting {
            async fn resolve(&self, _: &PermissionAsk) -> PermissionVerdict {
                self.0.fetch_add(1, Ordering::SeqCst);
                PermissionVerdict::Await {
                    correlation_id: "original-ticket".into(),
                }
            }
        }
        for ending in ["ack", "late-permission", "closed", "malformed", "timeout"] {
            let (mut ours, mut theirs) = tokio::io::duplex(8192);
            let calls = Arc::new(AtomicUsize::new(0));
            let resolver = Awaiting(calls.clone());
            let acknowledged = Arc::new(AtomicBool::new(false));
            let peer_acknowledged = acknowledged.clone();
            let (release, held) = tokio::sync::oneshot::channel();
            let agent = tokio::spawn(async move {
                let mut wire = Wire::new(&mut theirs);
                let permission = |id| {
                    serde_json::json!({"jsonrpc":"2.0","id":id,
                    "method":"session/request_permission","params":{"sessionId":"s",
                    "toolCall":{"toolCallId":"original","title":"write","rawInput":{"path":"proof"}},
                    "options":[{"optionId":"yes","name":"Allow","kind":"allow_once"},
                        {"optionId":"no","name":"Deny","kind":"reject_once"}]}})
                };
                wire.send(&permission(42)).await.unwrap();
                assert_eq!(
                    wire.read().await.unwrap().unwrap().result.unwrap()["outcome"]["outcome"],
                    "cancelled"
                );
                let cancel =
                    tokio::time::timeout(std::time::Duration::from_millis(250), wire.read())
                        .await
                        .expect("R1 cancellation must reach the agent")
                        .unwrap()
                        .unwrap();
                assert_eq!(cancel.method.as_deref(), Some("session/cancel"));
                assert_eq!(cancel.params.unwrap()["sessionId"], "s");
                match ending {
                    "closed" => return,
                    "timeout" => {
                        let _ = held.await;
                        return;
                    }
                    "malformed" => {
                        wire.send(&serde_json::json!({"id":3,"method":42}))
                            .await
                            .unwrap();
                        return;
                    }
                    "late-permission" => {
                        wire.send(&permission(43)).await.unwrap();
                        let result = wire.read().await.unwrap().unwrap().result.unwrap();
                        assert_eq!(result["outcome"]["optionId"], "no");
                    }
                    _ => {}
                }
                wire.send(
                    &serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                    "params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update",
                    "toolCallId":"original","status":"failed"}}}),
                )
                .await
                .unwrap();
                wire.send(
                    &serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                    "params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"late text must not commit"}}}}),
                )
                .await
                .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                peer_acknowledged.store(true, Ordering::SeqCst);
                wire.send(&serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}})).await.unwrap();
            });
            let mut sink = Sink::default();
            let outcome = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                pump_prompt(
                    &mut Wire::new(&mut ours),
                    &SessionId::new("s"),
                    &mut sink,
                    &mut 0,
                    &resolver,
                    std::time::Duration::from_millis(100),
                ),
            )
            .await
            .expect("R5 drain must return while peer remains open");
            let _ = release.send(());
            if matches!(ending, "ack" | "late-permission") {
                assert!(
                    acknowledged.load(Ordering::SeqCst),
                    "R1 must observe acknowledgement before returning"
                );
            }
            assert!(
                matches!(outcome, Err(AcpError::PermissionAwait { correlation_id, ask })
                if correlation_id == "original-ticket" && ask.call_id == "original" && ask.arguments["path"] == "proof")
            );
            agent.await.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "late requests never reuse policy/grants"
            );
            assert_eq!(
                sink.0.len(),
                1,
                "only original requested operation is retained"
            );
        }
    }

    #[tokio::test]
    async fn permission_settlement_retains_only_prior_unsettled_sibling_results() {
        // Cause/effect graph: a prior observed sibling + no terminal result +
        // exact unchanged input + a different Await call -> retain the sibling's
        // late terminal fact once. S1 completed/failed siblings survive; S2 the
        // Await target, unknown/late calls, duplicate results, changed arguments
        // and text never cross settlement. S3 late permission remains denied,
        // original ticket unchanged. All effects flow through the same sink and
        // monotonic sequence; no synthetic ToolResult or second commit owner.
        for status in ["completed", "failed"] {
            let (mut ours, mut theirs) = tokio::io::duplex(16384);
            struct Ask;
            #[async_trait::async_trait]
            impl PermissionResolver for Ask {
                async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
                    assert_eq!(
                        ask.call_id, "await",
                        "S4 foreign permission cannot reach policy"
                    );
                    PermissionVerdict::Await {
                        correlation_id: "ticket".into(),
                    }
                }
            }
            let peer = tokio::spawn(async move {
                let mut wire = Wire::new(&mut theirs);
                wire.send(&serde_json::json!({"jsonrpc":"2.0","id":41,"method":"session/request_permission",
                    "params":{"sessionId":"foreign","toolCall":{"toolCallId":"foreign","title":"bash","rawInput":{"command":"foreign"}},
                    "options":[{"optionId":"yes","name":"Allow","kind":"allow_once"}]}})).await.unwrap();
                assert_eq!(
                    wire.read().await.unwrap().unwrap().result.unwrap()["outcome"]["outcome"],
                    "cancelled"
                );
                for id in ["read", "changed", "settled", "reused"] {
                    wire.send(
                        &serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                        "params":{"sessionId":"s","update":{"sessionUpdate":"tool_call",
                        "toolCallId":id,"title":"read","rawInput":{"path":"SKILL.md"}}}}),
                    )
                    .await
                    .unwrap();
                }
                wire.send(&serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                    "params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"settled","status":"completed",
                    "content":[{"type":"content","content":{"type":"text","text":"earlier result"}}]}}})).await.unwrap();
                wire.send(&serde_json::json!({"jsonrpc":"2.0","id":42,"method":"session/request_permission",
                    "params":{"sessionId":"s","toolCall":{"toolCallId":"await","title":"bash","rawInput":{"command":"work"}},
                    "options":[{"optionId":"yes","name":"Allow","kind":"allow_once"}]}})).await.unwrap();
                assert_eq!(
                    wire.read().await.unwrap().unwrap().result.unwrap()["outcome"]["outcome"],
                    "cancelled"
                );
                assert_eq!(
                    wire.read().await.unwrap().unwrap().method.as_deref(),
                    Some("session/cancel")
                );
                for id in ["reused", "late"] {
                    wire.send(&serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                        "params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":id,"title":"read","rawInput":{"path":"SKILL.md"}}}})).await.unwrap();
                }
                // S4 a foreign Session cannot close the same wire call id.
                wire.send(&serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                    "params":{"sessionId":"foreign","update":{"sessionUpdate":"tool_call_update","toolCallId":"read","status":"completed",
                    "content":[{"type":"content","content":{"type":"text","text":"foreign result"}}]}}})).await.unwrap();
                for (id, input) in [
                    ("await", None),
                    ("unknown", None),
                    ("changed", Some(serde_json::json!({"path":"other"}))),
                    ("changed", None),
                    ("settled", None),
                    ("reused", None),
                    ("late", None),
                    ("read", Some(serde_json::json!({"path":"SKILL.md"}))),
                    ("read", None),
                ] {
                    let mut update = serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":status,
                        "content":[{"type":"content","content":{"type":"text","text":"exact sibling result"}}]});
                    if let Some(input) = input {
                        update["rawInput"] = input;
                    }
                    wire.send(&serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":update}})).await.unwrap();
                }
                wire.send(&serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}})).await.unwrap();
            });
            let mut sink = Sink::default();
            let mut seq = 0;
            let outcome = pump_prompt(
                &mut Wire::new(&mut ours),
                &SessionId::new("s"),
                &mut sink,
                &mut seq,
                &Ask,
                std::time::Duration::from_secs(1),
            )
            .await;
            assert!(
                matches!(outcome,Err(AcpError::PermissionAwait{correlation_id,ask}) if correlation_id=="ticket" && ask.call_id=="await")
            );
            peer.await.unwrap();
            assert_eq!(
                seq, 7,
                "five original calls, prior result, exact sibling result"
            );
            assert_eq!(
                sink.0.last(),
                Some(&AcpProjectedEvent::ToolResult {
                    id: "read".into(),
                    content: "exact sibling result".into(),
                    is_error: status == "failed"
                })
            );
            assert_eq!(sink.0.len(), 7);
        }
    }

    #[tokio::test]
    async fn streamed_arguments_refine_only_the_exact_call_before_its_result() {
        // C1 known nonempty call id; C2 absent/null/present rawInput; C3 same or
        // changed arguments; C4 terminal result in the same frame. R1 !C1 ->
        // no invented call; R2 absent/null/unchanged -> no duplicate fact; R3
        // changed input -> refinement under original tool identity; R4 R3+C4 ->
        // refinement precedes result. Progress titles cannot rename authority.
        let mut context = PermissionContext::default();
        let mut sink = Sink::default();
        let mut seq = 0;
        let frames = [
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"unknown","rawInput":{"x":1}}),
            serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"c","title":"bash","rawInput":{}}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","status":"in_progress"}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","rawInput":null}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","rawInput":{}}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","title":"Running command","rawInput":{"command":"first"}}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"c","rawInput":{"command":"final"},"status":"completed","content":[{"type":"content","content":{"type":"text","text":"done"}}]}),
        ];
        for update in frames {
            project_notification(
                Some(serde_json::json!({"sessionId":"s","update":update})),
                &mut sink,
                &mut seq,
                &mut context,
            )
            .await
            .unwrap();
        }
        assert_eq!(seq, 4);
        assert_eq!(
            sink.0,
            vec![
                AcpProjectedEvent::ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    input: serde_json::json!({})
                },
                AcpProjectedEvent::ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command":"first"})
                },
                AcpProjectedEvent::ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command":"final"})
                },
                AcpProjectedEvent::ToolResult {
                    id: "c".into(),
                    content: "done".into(),
                    is_error: false
                },
            ]
        );
    }
}
