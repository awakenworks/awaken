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
    match pump_response(wire, target_id, sink, seq, resolver).await? {
        RpcResponse::Result(result) => Ok(result),
        RpcResponse::Error(error) => Err(AcpError::Frame(error.to_string())),
    }
}

pub(super) enum RpcResponse {
    Result(serde_json::Value),
    Error(serde_json::Value),
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
    // Some ACP adapters (notably Codex) report the precise MCP identity only in
    // the preceding tool_call update, then ask permission for a generic
    // `execute` operation. The ACP toolCallId is the protocol correlation key;
    // retain the already-projected identity for the lifetime of this response
    // pump so the one neutral policy sees the real tool rather than a lossy title.
    let mut permission_context = PermissionContext::default();
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
                    &permission_context,
                )
                .await?;
            }
            // A notification (no id).
            None => {
                if msg.method.as_deref() == Some(CLIENT_METHOD_NAMES.session_update) {
                    project_notification(msg.params, sink, seq, &mut permission_context).await?;
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
