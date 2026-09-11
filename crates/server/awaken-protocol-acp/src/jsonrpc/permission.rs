//! Agent-to-client permission request handling for the ACP JSON-RPC driver.

use super::*;
use crate::{PermissionConsensus, permission_consensus_class};

/// Shared fail-closed policy for prompt-free discovery and cancelled-turn drain.
pub(super) struct RejectPermission;
#[async_trait::async_trait]
impl PermissionResolver for RejectPermission {
    async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
        PermissionVerdict::Deny
    }
}

#[derive(Clone)]
struct ObservedToolCall {
    name: String,
    input: serde_json::Value,
    // Disposable projection guard: closed by a terminal fact or an ambiguous
    // post-Await identity change, never a durable execution-state assertion.
    result_open: bool,
}

/// Attempt-local permission facts already projected from ACP `session/update`.
/// The map is keyed only by ACP's exact `toolCallId`, so a generic permission
/// title can recover its real identity without widening authority to another call.
#[derive(Default)]
pub(super) struct PermissionContext {
    observed_tools: std::collections::BTreeMap<String, ObservedToolCall>,
    settling_for: Option<String>,
    session_id: Option<String>,
}

impl PermissionContext {
    pub(super) fn for_session(session_id: &SessionId) -> Self {
        Self {
            session_id: Some(session_id.0.to_string()),
            ..Self::default()
        }
    }

    pub(super) fn accepts_session(&self, session_id: &SessionId) -> bool {
        self.session_id
            .as_deref()
            .is_none_or(|expected| expected == session_id.0.as_ref())
    }

    pub(super) fn observe(&mut self, event: &AcpProjectedEvent) {
        if let AcpProjectedEvent::ToolCall { id, name, input } = event
            && !id.is_empty()
        {
            self.observed_tools.insert(
                id.clone(),
                ObservedToolCall {
                    name: name.clone(),
                    input: crate::exact_mcp_tool_arguments(name, input),
                    result_open: true,
                },
            );
        }
        if let AcpProjectedEvent::ToolResult { id, .. } = event
            && let Some(observed) = self.observed_tools.get_mut(id)
        {
            observed.result_open = false;
        }
    }

    pub(super) fn begin_settlement(&mut self, pending_call: &str) {
        self.settling_for = Some(pending_call.to_owned());
    }

    pub(super) fn is_settling(&self) -> bool {
        self.settling_for.is_some()
    }

    pub(super) fn settlement_result(
        &mut self,
        update: &agent_client_protocol::SessionUpdate,
    ) -> Option<AcpProjectedEvent> {
        if let Some(AcpProjectedEvent::ToolCall { id, .. }) = project_update(update) {
            if let Some(observed) = self.observed_tools.get_mut(&id) {
                observed.result_open = false;
            }
            return None;
        }
        let agent_client_protocol::SessionUpdate::ToolCallUpdate(update_call) = update else {
            return None;
        };
        let id = update_call.tool_call_id.0.as_ref();
        let observed = self.matching_observed_tool(id)?;
        if self.settling_for.as_deref() == Some(id) || !observed.result_open {
            return None;
        }
        if self.refined_call(update_call).is_some() {
            // A later frame without rawInput must not erase the conflict.
            self.observed_tools.get_mut(id)?.result_open = false;
            return None;
        }
        project_update(update).filter(|event| matches!(event, AcpProjectedEvent::ToolResult { .. }))
    }

    /// Streaming progress refines only a previously observed exact call. Titles
    /// on progress updates are presentation text, so retain its original identity.
    pub(super) fn refined_call(
        &self,
        update: &agent_client_protocol::ToolCallUpdate,
    ) -> Option<AcpProjectedEvent> {
        let id = update.tool_call_id.0.as_ref();
        let observed = self.matching_observed_tool(id)?;
        let input = update
            .fields
            .raw_input
            .as_ref()
            .filter(|input| !input.is_null())?;
        let input = crate::exact_mcp_tool_arguments(&observed.name, input);
        (input != observed.input).then(|| AcpProjectedEvent::ToolCall {
            id: id.to_owned(),
            name: observed.name.clone(),
            input,
        })
    }

    fn matching_observed_tool(&self, call_id: &str) -> Option<&ObservedToolCall> {
        let candidate = self.observed_tools.get_key_value(call_id);
        let requested_id = (!call_id.is_empty()).then_some(call_id);
        let observed_id = candidate.map(|(observed_id, _)| observed_id.as_str());
        match normalize_tool_identity_source(requested_id, observed_id) {
            ToolIdentitySource::Observed => candidate.map(|(_, observed)| observed),
            ToolIdentitySource::Raw => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolIdentitySource {
    Raw,
    Observed,
}

/// Select the more precise observed identity only for one exact, non-empty ACP
/// `toolCallId`. Absence, an empty request id, and any unequal id all retain the
/// request's raw identity, so an unrelated call can never lend its name/input.
fn normalize_tool_identity_source<Id: PartialEq>(
    requested_id: Option<Id>,
    observed_id: Option<Id>,
) -> ToolIdentitySource {
    match (requested_id, observed_id) {
        (Some(requested_id), Some(observed_id)) if requested_id == observed_id => {
            ToolIdentitySource::Observed
        }
        (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) | (None, None) => {
            ToolIdentitySource::Raw
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionEffect {
    ProjectCall,
    ResolveAuthority,
    SendResponse,
    NoEffect,
}

/// The sole local effect plan for one valid ACP permission request. Normal
/// prompt handling first projects the exact call; settlement has already closed
/// projection and therefore starts at the same authority-resolution edge.
const fn permission_effect_plan(project_call: bool) -> [PermissionEffect; 3] {
    if project_call {
        [
            PermissionEffect::ProjectCall,
            PermissionEffect::ResolveAuthority,
            PermissionEffect::SendResponse,
        ]
    } else {
        [
            PermissionEffect::ResolveAuthority,
            PermissionEffect::SendResponse,
            PermissionEffect::NoEffect,
        ]
    }
}

#[cfg(kani)]
#[kani::proof]
fn acp_permission_effect_plan_preserves_authority_order() {
    // Cause/effect decision table for the production-consumed local plan:
    // | project open | first effect      | second effect     | third effect |
    // | true         | project exact call | resolve authority | send response |
    // | false        | resolve authority  | send response     | none          |
    // Therefore every response follows exactly one authority resolution, and
    // an enabled projection precedes both. Async completion and remote effects
    // are intentionally outside this allocation-free sequencing proof.
    let project_call: bool = kani::any();
    let plan = permission_effect_plan(project_call);
    if project_call {
        assert_eq!(
            plan,
            [
                PermissionEffect::ProjectCall,
                PermissionEffect::ResolveAuthority,
                PermissionEffect::SendResponse,
            ]
        );
    } else {
        assert_eq!(
            plan,
            [
                PermissionEffect::ResolveAuthority,
                PermissionEffect::SendResponse,
                PermissionEffect::NoEffect,
            ]
        );
    }
}

/// Answer an agent→client request: a permission request is decided by `resolver`
/// (the neutral `ToolPermissionPolicy`) and projected back onto the agent's own
/// offered option — allow or reject, once-preferred over always; `cancelled` when
/// no matching option is offered. Every other method — the `fs`/`terminal`
/// capabilities we never advertised — gets `method_not_found`, because tool
/// execution is the hand's job and is never proxied back over ACP.
pub(super) async fn answer_request(
    wire: &mut Wire<'_>,
    id: serde_json::Value,
    method: &str,
    params: Option<serde_json::Value>,
    resolver: &dyn PermissionResolver,
    context: &mut PermissionContext,
    projection: (&mut dyn RunFactAppender, &mut u64),
) -> Result<(), AcpError> {
    if method != CLIENT_METHOD_NAMES.session_request_permission {
        return wire
            .send(&OutError {
                jsonrpc: JSONRPC,
                id,
                error: RpcErrorBody {
                    code: METHOD_NOT_FOUND,
                    message: "capability not supported by this client",
                },
            })
            .await;
    }
    let parsed = params.and_then(|raw| {
        parse::<RequestPermissionRequest>(raw.clone())
            .ok()
            .map(|request| (raw, request))
    });
    let Some((raw, request)) =
        parsed.filter(|(_, request)| context.accepts_session(&request.session_id))
    else {
        return wire
            .send(&OutResult {
                jsonrpc: JSONRPC,
                id,
                result: RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
            })
            .await;
    };
    let ask = permission_ask(&raw, context);
    let mut verdict = None;
    for effect in permission_effect_plan(!context.is_settling()) {
        match effect {
            PermissionEffect::ProjectCall => {
                // Permission carries the resolved target and complete execution
                // input. Refine the same call before policy/dispatch so transcript,
                // one-shot grant and later streaming results share one identity.
                let event = AcpProjectedEvent::ToolCall {
                    id: ask.call_id.clone(),
                    name: ask.tool.clone(),
                    input: ask.arguments.clone(),
                };
                *projection.1 += 1;
                projection.0.append(*projection.1, &event).await?;
                context.observe(&event);
            }
            PermissionEffect::ResolveAuthority => {
                verdict = Some(resolver.resolve(&ask).await);
            }
            PermissionEffect::SendResponse => {
                let verdict = verdict.take().ok_or_else(|| {
                    AcpError::Frame("ACP permission response preceded authority resolution".into())
                })?;
                let (outcome, awaiting) = match verdict {
                    PermissionVerdict::Await { correlation_id } => {
                        (RequestPermissionOutcome::Cancelled, Some(correlation_id))
                    }
                    verdict => (select_outcome(&request, verdict), None),
                };
                wire.send(&OutResult {
                    jsonrpc: JSONRPC,
                    id: id.clone(),
                    result: RequestPermissionResponse::new(outcome),
                })
                .await?;
                return match awaiting {
                    Some(correlation_id) => Err(AcpError::PermissionAwait {
                        correlation_id,
                        ask: Box::new(ask.clone()),
                    }),
                    None => Ok(()),
                };
            }
            PermissionEffect::NoEffect => {}
        }
    }
    Err(AcpError::Frame(
        "ACP permission effect plan ended before response".into(),
    ))
}

/// Project a raw `session/request_permission` params object into a neutral
/// [`PermissionAsk`] — the tool's title/kind, its `toolCallId`, and its `rawInput`
/// — read loosely so the ask survives adapter-to-adapter shape differences.
fn permission_ask(raw: &serde_json::Value, context: &PermissionContext) -> PermissionAsk {
    let tool_call = raw.get("toolCall");
    let tool = tool_call
        .and_then(|tc| tc.get("title").or_else(|| tc.get("kind")))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let call_id = tool_call
        .and_then(|tc| tc.get("toolCallId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let arguments = tool_call
        .and_then(|tc| tc.get("rawInput"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let mut ask = PermissionAsk {
        tool,
        call_id,
        arguments,
    };
    if let Some(observed) = context.matching_observed_tool(&ask.call_id) {
        // Claude reports alias display text during streaming, then the resolved
        // MCP name at permission time. Explicit MCP requests own that identity;
        // generic requests (e.g. Codex execute) still use exact-id observation.
        // Retain even malformed MCP-looking requests for downstream rejection.
        if !ask.tool.starts_with("mcp__") && !ask.tool.starts_with("mcp.") {
            ask.tool.clone_from(&observed.name);
        }
        // Streaming tool_call notifications may still contain {} or partial
        // fields. The permission request is the execution-boundary input; an
        // exact prior observation supplies arguments only when that input is
        // absent/null (as with adapters that send a generic execution request).
        if ask.arguments.is_null() {
            ask.arguments.clone_from(&observed.input);
        }
    }
    ask.arguments = crate::exact_mcp_tool_arguments(&ask.tool, &ask.arguments);
    ask
}

#[cfg(kani)]
#[kani::proof]
fn acp_permission_tool_identity_normalization_is_exact() {
    let requested_value: u16 = kani::any();
    let observed_value: u16 = kani::any();
    let requested_nonempty: bool = kani::any();
    let observed_present: bool = kani::any();

    let requested_id = requested_nonempty.then_some(requested_value);
    let observed_id = observed_present.then_some(observed_value);
    let selected = normalize_tool_identity_source(requested_id, observed_id);
    let exact_nonempty_match =
        requested_nonempty && observed_present && requested_value == observed_value;

    assert_eq!(
        selected == ToolIdentitySource::Observed,
        exact_nonempty_match
    );
    if !requested_nonempty || !observed_present || requested_value != observed_value {
        assert_eq!(selected, ToolIdentitySource::Raw);
    }
}

/// Project a [`PermissionVerdict`] onto the agent's own offered option: an
/// `Allow` picks `allow_once` (else `allow_always`); a `Deny` picks `reject_once`
/// (else `reject_always`). When the agent offered no option of the decided kind
/// the turn is cancelled (a well-behaved agent always offers both).
fn select_outcome(
    req: &RequestPermissionRequest,
    verdict: PermissionVerdict,
) -> RequestPermissionOutcome {
    let offered = |kind| req.options.iter().any(|option| option.kind == kind);
    let selected_kind = project_permission_option(
        permission_consensus_class(&verdict),
        offered(PermissionOptionKind::AllowOnce),
        offered(PermissionOptionKind::AllowAlways),
        offered(PermissionOptionKind::RejectOnce),
        offered(PermissionOptionKind::RejectAlways),
    );
    let chosen = selected_kind
        .permission_kind()
        .and_then(|kind| req.options.iter().find(|option| option.kind == kind));
    match chosen {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpPermissionProjection {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Cancelled,
}

impl AcpPermissionProjection {
    const fn permission_kind(self) -> Option<PermissionOptionKind> {
        match self {
            Self::AllowOnce => Some(PermissionOptionKind::AllowOnce),
            Self::AllowAlways => Some(PermissionOptionKind::AllowAlways),
            Self::RejectOnce => Some(PermissionOptionKind::RejectOnce),
            Self::RejectAlways => Some(PermissionOptionKind::RejectAlways),
            Self::Cancelled => None,
        }
    }
}

/// Total, allocation-free projection from a neutral verdict and the exact ACP
/// offer set. `Await` and a missing same-polarity option both fail closed to
/// cancellation; an opposite-polarity option can never satisfy the verdict.
const fn project_permission_option(
    verdict: PermissionConsensus,
    allow_once: bool,
    allow_always: bool,
    reject_once: bool,
    reject_always: bool,
) -> AcpPermissionProjection {
    match verdict {
        PermissionConsensus::Allow if allow_once => AcpPermissionProjection::AllowOnce,
        PermissionConsensus::Allow if allow_always => AcpPermissionProjection::AllowAlways,
        PermissionConsensus::Deny if reject_once => AcpPermissionProjection::RejectOnce,
        PermissionConsensus::Deny if reject_always => AcpPermissionProjection::RejectAlways,
        PermissionConsensus::Allow | PermissionConsensus::Await | PermissionConsensus::Deny => {
            AcpPermissionProjection::Cancelled
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn acp_permission_projection_is_total_exact_and_non_widening() {
    let verdict = crate::arbitrary_permission_consensus(kani::any(), kani::any());
    let allow_once: bool = kani::any();
    let allow_always: bool = kani::any();
    let reject_once: bool = kani::any();
    let reject_always: bool = kani::any();
    let projected = project_permission_option(
        verdict,
        allow_once,
        allow_always,
        reject_once,
        reject_always,
    );

    let expected = match verdict {
        PermissionConsensus::Allow if allow_once => AcpPermissionProjection::AllowOnce,
        PermissionConsensus::Allow if allow_always => AcpPermissionProjection::AllowAlways,
        PermissionConsensus::Deny if reject_once => AcpPermissionProjection::RejectOnce,
        PermissionConsensus::Deny if reject_always => AcpPermissionProjection::RejectAlways,
        PermissionConsensus::Allow | PermissionConsensus::Await | PermissionConsensus::Deny => {
            AcpPermissionProjection::Cancelled
        }
    };
    assert_eq!(projected, expected);
    match verdict {
        PermissionConsensus::Allow => assert!(!matches!(
            projected,
            AcpPermissionProjection::RejectOnce | AcpPermissionProjection::RejectAlways
        )),
        PermissionConsensus::Await => {
            assert_eq!(projected, AcpPermissionProjection::Cancelled)
        }
        PermissionConsensus::Deny => assert!(!matches!(
            projected,
            AcpPermissionProjection::AllowOnce | AcpPermissionProjection::AllowAlways
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type EffectLog = std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>;

    struct EffectSink {
        log: EffectLog,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl RunFactAppender for EffectSink {
        async fn append(
            &mut self,
            _seq: u64,
            _event: &AcpProjectedEvent,
        ) -> Result<(), AppendError> {
            self.log.lock().unwrap().push("project");
            if self.fail {
                Err(AppendError::Append("injected projection failure".into()))
            } else {
                Ok(())
            }
        }
    }

    struct EffectResolver {
        log: EffectLog,
        verdict: PermissionVerdict,
    }

    #[async_trait::async_trait]
    impl PermissionResolver for EffectResolver {
        async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
            self.log.lock().unwrap().push("resolve");
            self.verdict.clone()
        }
    }

    struct EffectChannel {
        log: EffectLog,
        fail: bool,
    }

    impl tokio::io::AsyncRead for EffectChannel {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl tokio::io::AsyncWrite for EffectChannel {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let channel = self.get_mut();
            channel.log.lock().unwrap().push("send");
            if channel.fail {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected response failure",
                )))
            } else {
                std::task::Poll::Ready(Ok(buf.len()))
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn permission_effect_plan_orders_projection_authority_and_response() {
        // Cause/effect table over answer_request and its production effect plan:
        // | rule | valid | projection open | append | verdict | send | effects |
        // | R1 | yes | yes | ok | allow | ok | project, resolve, send; success |
        // | R2 | yes | yes | ok | await | ok | project, resolve, send; Await |
        // | R3 | yes | no  | n/a| deny  | ok | resolve, send; success |
        // | R4 | yes | yes | fail | any | n/a | project only; append error |
        // | R5 | yes | yes | ok | deny | fail | project, resolve, send; I/O error |
        // | R6 | no  | any | n/a | n/a | ok | send cancelled; no authority call |
        // Effects are observed at the actual appender, resolver and Wire writer.
        // Tokio scheduling and what a remote peer does after write remain TCB.
        let request = serde_json::json!({
            "sessionId":"s",
            "toolCall":{"toolCallId":"call","title":"bash","rawInput":{"cmd":"echo ok"}},
            "options":[
                {"optionId":"allow","name":"Allow","kind":"allow_once"},
                {"optionId":"deny","name":"Deny","kind":"reject_once"}
            ]
        });
        for (rule, valid, settling, append_fails, verdict, send_fails, expected, outcome) in [
            (
                "R1",
                true,
                false,
                false,
                PermissionVerdict::Allow,
                false,
                vec!["project", "resolve", "send"],
                "ok",
            ),
            (
                "R2",
                true,
                false,
                false,
                PermissionVerdict::Await {
                    correlation_id: "ticket".into(),
                },
                false,
                vec!["project", "resolve", "send"],
                "await",
            ),
            (
                "R3",
                true,
                true,
                false,
                PermissionVerdict::Deny,
                false,
                vec!["resolve", "send"],
                "ok",
            ),
            (
                "R4",
                true,
                false,
                true,
                PermissionVerdict::Allow,
                false,
                vec!["project"],
                "append",
            ),
            (
                "R5",
                true,
                false,
                false,
                PermissionVerdict::Deny,
                true,
                vec!["project", "resolve", "send"],
                "io",
            ),
            (
                "R6",
                false,
                false,
                false,
                PermissionVerdict::Allow,
                false,
                vec!["send"],
                "ok",
            ),
        ] {
            let log = EffectLog::default();
            let resolver = EffectResolver {
                log: log.clone(),
                verdict,
            };
            let mut sink = EffectSink {
                log: log.clone(),
                fail: append_fails,
            };
            let mut channel = EffectChannel {
                log: log.clone(),
                fail: send_fails,
            };
            let mut context = PermissionContext::for_session(&SessionId::new("s"));
            if settling {
                context.begin_settlement("prior");
            }
            let params = valid
                .then(|| request.clone())
                .or_else(|| Some(serde_json::json!({"sessionId":"s","invalid":true})));
            let result = answer_request(
                &mut Wire::new(&mut channel),
                serde_json::json!(42),
                CLIENT_METHOD_NAMES.session_request_permission,
                params,
                &resolver,
                &mut context,
                (&mut sink, &mut 0),
            )
            .await;
            assert_eq!(*log.lock().unwrap(), expected, "{rule}");
            assert!(
                match outcome {
                    "ok" => result.is_ok(),
                    "await" => matches!(result, Err(AcpError::PermissionAwait { .. })),
                    "append" => matches!(result, Err(AcpError::Append(_))),
                    "io" => matches!(result, Err(AcpError::Io(_))),
                    _ => false,
                },
                "{rule}: {result:?}"
            );
        }
    }

    #[test]
    fn permission_ask_uses_only_the_matching_observed_tool() {
        // Cause/effect graph: C1 a tool_call carries precise MCP facts; C2 the
        // permission request carries a generic title; C3 ids match. E1 is the
        // precise observed identity/input. Without C3, E2 is the raw request,
        // preventing one call from lending authority to another.
        //
        // | Rule | Observed call | Same id | Effect |
        // | R1 | yes | yes | observed identity/input |
        // | R2 | yes | no | raw permission facts |
        let mut context = PermissionContext::default();
        context.observe(&AcpProjectedEvent::ToolCall {
            id: "mcp-1".into(),
            name: "mcp.pilot.set_plan".into(),
            input: serde_json::json!({
                "server": "pilot",
                "tool": "set_plan",
                "arguments": {"summary": "ship"}
            }),
        });
        let raw = |id: &str| {
            serde_json::json!({
                "toolCall": {"toolCallId": id, "title": "execute", "rawInput": null}
            })
        };

        let matched = permission_ask(&raw("mcp-1"), &context);
        assert_eq!(matched.tool, "mcp.pilot.set_plan", "R1");
        assert_eq!(matched.arguments["summary"], "ship", "R1");

        context.observe(&AcpProjectedEvent::ToolCall {
            id: "mcp-mismatch".into(),
            name: "mcp.pilot.set_plan".into(),
            input: serde_json::json!({
                "server": "other",
                "tool": "set_plan",
                "arguments": {"summary": "unsafe"}
            }),
        });
        let mismatch = permission_ask(&raw("mcp-mismatch"), &context);
        assert_eq!(
            mismatch.arguments["server"], "other",
            "identity mismatch fails closed"
        );

        let unrelated = permission_ask(&raw("other"), &context);
        assert_eq!(unrelated.tool, "execute", "R2");
        assert!(unrelated.arguments.is_null(), "R2");

        let empty = permission_ask(&raw(""), &context);
        assert_eq!(empty.tool, "execute", "empty ids fail closed");
        assert!(empty.arguments.is_null(), "empty ids fail closed");
    }

    #[test]
    fn resolved_mcp_permission_identity_supersedes_alias_display_title() {
        // Identity graph: request contains resolved MCP target (C1), exact-id
        // prior display notification (C2), complete request input (C3).
        // R1 C1+C2 -> resolved target, never the alias display title; R2 !C1+C2
        // -> existing observed identity for generic Codex requests; R3 C1+!C2
        // -> request identity. Both MCP encodings share these rules. Input C3
        // is authoritative in every row (including an explicit empty object).
        for target in ["mcp__awaken_session__read", "mcp.awaken_session.read"] {
            let mut context = PermissionContext::default();
            context.observe(&AcpProjectedEvent::ToolCall {
                id: "exact".into(),
                name: "Read File".into(),
                input: serde_json::json!({}),
            });
            for id in ["exact", "foreign"] {
                let ask = permission_ask(
                    &serde_json::json!({"toolCall": {
                        "toolCallId":id, "title":target,
                        "rawInput":{"file_path":"/mnt/session/uploads/brief.txt"}
                    }}),
                    &context,
                );
                assert_eq!(ask.tool, target, "R1/R3");
                assert_eq!(ask.arguments["file_path"], "/mnt/session/uploads/brief.txt");
            }
        }
    }

    /// Permission input decision table: I1 present request input supersedes an
    /// empty/partial/drifted streaming observation; I2 explicit {} is present,
    /// not a request to reuse old fields; I3 absent/null retains the exact-id
    /// observed fallback; I4 mismatched ids lend neither name nor arguments.
    /// Exact MCP envelopes use the existing normalizer after identity selection;
    /// a mismatched envelope is retained for fail-closed admission, not unwrapped.
    #[test]
    fn permission_request_owns_complete_arguments_after_streaming_placeholders() {
        for observed_input in [serde_json::json!({}), serde_json::json!({"command":"old"})] {
            let mut context = PermissionContext::default();
            context.observe(&AcpProjectedEvent::ToolCall {
                id: "exact".into(),
                name: "mcp__awaken_session__bash".into(),
                input: observed_input.clone(),
            });
            for requested in [
                serde_json::json!({"command":"echo approved", "timeout":30}),
                serde_json::json!({}),
            ] {
                let ask = permission_ask(
                    &serde_json::json!({"toolCall":{"toolCallId":"exact","title":"execute","rawInput":requested}}),
                    &context,
                );
                assert_eq!(ask.tool, "mcp__awaken_session__bash", "I1 identity");
                assert_eq!(ask.arguments, requested, "I1/I2 exact request input");
            }
            for raw in [
                serde_json::json!({"toolCallId":"exact","title":"execute"}),
                serde_json::json!({"toolCallId":"exact","title":"execute","rawInput":null}),
            ] {
                let ask = permission_ask(&serde_json::json!({"toolCall":raw}), &context);
                assert_eq!(ask.arguments, observed_input, "I3");
            }
            let ask = permission_ask(
                &serde_json::json!({"toolCall":{"toolCallId":"foreign","title":"read","rawInput":{"path":"file"}}}),
                &context,
            );
            assert_eq!(ask.tool, "read", "I4");
            assert_eq!(ask.arguments, serde_json::json!({"path":"file"}), "I4");
        }
        let mut context = PermissionContext::default();
        context.observe(&AcpProjectedEvent::ToolCall {
            id: "mcp".into(),
            name: "mcp.pilot.set_plan".into(),
            input: serde_json::json!({}),
        });
        for server in ["pilot", "foreign"] {
            let envelope = serde_json::json!({"server":server,"tool":"set_plan","arguments":{"summary":"ship"}});
            let ask = permission_ask(
                &serde_json::json!({"toolCall":{"toolCallId":"mcp","title":"execute","rawInput":envelope}}),
                &context,
            );
            assert_eq!(
                ask.arguments,
                if server == "pilot" {
                    serde_json::json!({"summary":"ship"})
                } else {
                    envelope
                }
            );
        }
    }

    fn perm_req(options: serde_json::Value) -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "sessionId": "sess-1",
            "toolCall": { "toolCallId": "t1" },
            "options": options,
        }))
        .expect("a valid permission request")
    }

    fn outcome_json(outcome: &RequestPermissionOutcome) -> String {
        serde_json::to_value(outcome).unwrap().to_string()
    }

    #[test]
    fn permission_outcome_selection_is_complete_and_fail_closed() {
        // FMECA cause/effect graph: C1=neutral verdict {allow,deny}; C2=the ACP
        // request offers the matching once option; C3=it offers only the matching
        // always option; C4=it offers no matching option. Effects: E1=select the
        // matching once id (least persistent grant); E2=fall back to the matching
        // always id; E3=cancel rather than select an opposite-kind option.
        //
        // | Rule | C1    | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|---|
        // | PS1 | allow | T | * | F | E1 `allow-once` |
        // | PS2 | allow | F | T | F | E2 `allow-always` |
        // | PS3 | allow | F | F | T | E3 cancelled |
        // | PS4 | deny  | T | * | F | E1 `reject-once` |
        // | PS5 | deny  | F | T | F | E2 `reject-always` |
        // | PS6 | deny  | F | F | T | E3 cancelled |
        // `Await` is constrained out of this selector: `answer_request` owns that
        // branch and its wire-cancel + durable-ticket behavior is covered by
        // `an_awaiting_policy_cancels_the_wire_request_and_surfaces_the_neutral_ask`
        // and the executor-replacement FMECA test.
        let all_options = serde_json::json!([
            {"optionId":"allow-once","name":"Allow once","kind":"allow_once"},
            {"optionId":"allow-always","name":"Allow always","kind":"allow_always"},
            {"optionId":"reject-once","name":"Reject once","kind":"reject_once"},
            {"optionId":"reject-always","name":"Reject always","kind":"reject_always"},
        ]);
        let cases = [
            (
                "PS1",
                PermissionVerdict::Allow,
                all_options.clone(),
                Some("allow-once"),
            ),
            (
                "PS2",
                PermissionVerdict::Allow,
                serde_json::json!([
                    {"optionId":"allow-always","name":"Allow always","kind":"allow_always"},
                    {"optionId":"reject-once","name":"Reject once","kind":"reject_once"},
                ]),
                Some("allow-always"),
            ),
            (
                "PS3",
                PermissionVerdict::Allow,
                serde_json::json!([
                    {"optionId":"reject-once","name":"Reject once","kind":"reject_once"},
                ]),
                None,
            ),
            (
                "PS4",
                PermissionVerdict::Deny,
                all_options,
                Some("reject-once"),
            ),
            (
                "PS5",
                PermissionVerdict::Deny,
                serde_json::json!([
                    {"optionId":"allow-once","name":"Allow once","kind":"allow_once"},
                    {"optionId":"reject-always","name":"Reject always","kind":"reject_always"},
                ]),
                Some("reject-always"),
            ),
            (
                "PS6",
                PermissionVerdict::Deny,
                serde_json::json!([
                    {"optionId":"allow-once","name":"Allow once","kind":"allow_once"},
                ]),
                None,
            ),
        ];

        for (rule, verdict, options, expected_id) in cases {
            let outcome = select_outcome(&perm_req(options), verdict);
            match expected_id {
                Some(expected_id) => {
                    assert!(
                        matches!(outcome, RequestPermissionOutcome::Selected(_)),
                        "{rule}: {outcome:?}"
                    );
                    assert!(outcome_json(&outcome).contains(expected_id), "{rule}");
                }
                None => assert!(
                    matches!(outcome, RequestPermissionOutcome::Cancelled),
                    "{rule}: {outcome:?}"
                ),
            }
        }
    }
}
