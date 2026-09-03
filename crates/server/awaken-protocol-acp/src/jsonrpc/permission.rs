//! Agent-to-client permission request handling for the ACP JSON-RPC driver.

use super::*;
use crate::{PermissionConsensus, permission_consensus_class};

#[derive(Clone)]
struct ObservedToolCall {
    name: String,
    input: serde_json::Value,
}

/// Attempt-local permission facts already projected from ACP `session/update`.
/// The map is keyed only by ACP's exact `toolCallId`, so a generic permission
/// title can recover its real identity without widening authority to another call.
#[derive(Default)]
pub(super) struct PermissionContext {
    observed_tools: std::collections::BTreeMap<String, ObservedToolCall>,
}

impl PermissionContext {
    pub(super) fn observe(&mut self, event: &AcpProjectedEvent) {
        if let AcpProjectedEvent::ToolCall { id, name, input } = event
            && !id.is_empty()
        {
            self.observed_tools.insert(
                id.clone(),
                ObservedToolCall {
                    name: name.clone(),
                    input: crate::exact_mcp_tool_arguments(name, input),
                },
            );
        }
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
    context: &PermissionContext,
) -> Result<(), AcpError> {
    if method == CLIENT_METHOD_NAMES.session_request_permission {
        let outcome = match params.and_then(|p| {
            parse::<RequestPermissionRequest>(p.clone())
                .ok()
                .map(|r| (p, r))
        }) {
            Some((raw, req)) => {
                let ask = permission_ask(&raw, context);
                match resolver.resolve(&ask).await {
                    PermissionVerdict::Await { correlation_id } => {
                        wire.send(&OutResult {
                            jsonrpc: JSONRPC,
                            id,
                            result: RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ),
                        })
                        .await?;
                        return Err(AcpError::PermissionAwait {
                            correlation_id,
                            ask: Box::new(ask),
                        });
                    }
                    verdict => select_outcome(&req, verdict),
                }
            }
            None => RequestPermissionOutcome::Cancelled,
        };
        return wire
            .send(&OutResult {
                jsonrpc: JSONRPC,
                id,
                result: RequestPermissionResponse::new(outcome),
            })
            .await;
    }
    wire.send(&OutError {
        jsonrpc: JSONRPC,
        id,
        error: RpcErrorBody {
            code: METHOD_NOT_FOUND,
            message: "capability not supported by this client",
        },
    })
    .await
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
        ask.tool.clone_from(&observed.name);
        ask.arguments.clone_from(&observed.input);
    }
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
