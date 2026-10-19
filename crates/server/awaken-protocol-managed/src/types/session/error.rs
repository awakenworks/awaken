// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Static owner: the two error shapes the Managed wire returns and commits, split out of
// @formal reason: types/session.rs. Dynamic path: a neutral runtime fault is classified into one SDK error
// @formal reason: variant carrying the retry status a client keys recovery on, and the HTTP envelope is
// @formal reason: built for a refused request; neither reads or writes execution state, and an
// @formal reason: unrecognized fault code falls back to unknown_error rather than claiming a retryable
// @formal reason: classification. Evidence: protocol.adapters.adapter_projection plus this module's
// @formal reason: classification tests; this is a product-requirement classification, not a direct proof
// @formal reason: claim.
//! The error shapes the Managed wire returns and commits.
//!
//! Two distinct things share the word "error" and are kept apart here. The
//! *fault* (`SessionError`, `RetryStatus`) is committed as the payload of a
//! `session.error` event: something went wrong while the session ran, and the
//! client keys recovery on whether retrying can help. The *envelope*
//! (`ErrorResponse`, `ApiError`) is what an HTTP request gets back when it is
//! refused; it never enters the event log. Classification of a neutral runtime
//! fault into a wire fault lives with the fault, so a caller cannot invent a
//! retry status beside it.

use awaken_managed_session_events::RunFault;
use serde::Serialize;

/// SDK: none — the HTTP error envelope is the API's own transport shape, not a
/// Managed resource the SDK declares as a type; the client parses it into its
/// `APIError` exception rather than a beta model.
///
/// The Anthropic error envelope: `{ "type": "error", "error": { "type", "message" } }`.
/// The SDK parses this shape to populate `err.error.type` / `err.error.message`;
/// `error.type` is the status-keyed discriminator (`not_found_error`,
/// `invalid_request_error`, `api_error`, …).
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub error: ApiError,
    pub request_id: Option<String>,
}

/// SDK: none — the inner object of the transport envelope above.
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
}

impl ErrorResponse {
    /// Build an envelope with `type: "error"` and the given inner error type.
    pub fn new(error_type: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: "error",
            error: ApiError {
                kind: error_type,
                message: message.into(),
            },
            request_id: None,
        }
    }
}

/// `BetaManagedAgentsRetryStatusRetrying` | `BetaManagedAgentsRetryStatusExhausted`
/// | `BetaManagedAgentsRetryStatusTerminal`: the SDK spells each status as its own
/// tagged object; one closed enum carries the same discriminator.
///
/// What a client should do next in response to a `session.error` — a tagged
/// object (the SDK's `BetaManagedAgentsRetryStatus` union), never a bare string:
/// `Retrying` (transient, the runtime will retry), `Exhausted` (retry budget spent,
/// but the session stays usable), or `Terminal` (retrying cannot help this fault).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RetryStatus {
    Retrying,
    Exhausted,
    Terminal,
}

/// The `error` object of a `session.error` event — one of the SDK's error variants
/// (`unknown_error` plus the classified `model_rate_limited_error` /
/// `model_request_failed_error`), each a human-readable `message` plus the
/// `retry_status` the client keys recovery on. The neutral runtime's stable fault
/// `code` selects the variant; the Session aggregate's terminal
/// `activation_failed` classification remains an `unknown_error` but is not
/// retryable. An unrecognized code honestly falls back to `unknown_error`.
/// `BetaManagedAgentsUnknownError` | `BetaManagedAgentsModelRateLimitedError` |
/// `BetaManagedAgentsModelRequestFailedError` and their siblings: the SDK
/// declares one interface per error `type`, all with the same fields, so one
/// struct whose `kind` carries the discriminator projects every variant.
#[derive(Debug, Clone, Serialize)]
pub struct SessionError {
    /// The SDK error type, e.g. `"unknown_error"` or `"model_rate_limited_error"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
    pub retry_status: RetryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_name: Option<String>,
}

impl SessionError {
    /// One classifier for committed root and child Run failures. An unknown
    /// physical result is not ordinary retry exhaustion and must not disappear
    /// behind an otherwise normal end_turn event.
    #[cfg(test)]
    pub(crate) fn from_run_state(
        state: &awaken_agent_contract::agent::run::RunState,
    ) -> Option<Self> {
        awaken_managed_session_events::run_state_fault(state).map(|fault| Self::from_fault(&fault))
    }

    /// The Managed error of one neutral Run fault.
    pub(crate) fn from_fault(fault: &RunFault) -> Self {
        match fault {
            RunFault::Failed { code, message } => Self::classify(code, message.clone()),
            RunFault::ExecutionUnknown => Self::execution_unknown(),
        }
    }

    pub(crate) fn execution_unknown() -> Self {
        Self {
            kind: "unknown_error",
            message: "The execution result is unknown. This operation will not be automatically repeated. Do not repeat external actions until their outcome is verified.".into(),
            retry_status: RetryStatus::Terminal,
            mcp_server_name: None,
        }
    }

    /// Classify a neutral runtime fault `(code, message)` into an SDK error object.
    /// The `code` is the stable snake_case class from `Failure::Inference`
    /// (`rate_limited` / `context_overflow` / `unauthorized` / …), or the Session
    /// aggregate's `activation_failed` state; it picks the error `type` and the
    /// `retry_status`. An unknown code (or a plain internal fault) is the catch-all
    /// `unknown_error` / `exhausted` — the session stays usable, so we don't
    /// force-terminate on one failed Run.
    pub fn classify(code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let mcp_server_name = code_message_server_name(code, &message);
        let (kind, retry_status) = match code {
            // Both mcp variants require `mcp_server_name` on the wire, and the
            // name is read out of the fault's prose below. A message without the
            // marker cannot satisfy the variant its code would otherwise select,
            // and claiming it anyway emits an event an official client cannot
            // deserialize, because the required property is simply absent. The
            // kind and the field are therefore decided in one expression: the
            // variant is reachable only when its required field exists, and a
            // nameless mcp fault degrades to `unknown_error`, which requires no
            // such field. The recovery the client keys on does not degrade with
            // it — an unreachable server is still retryable, a rejected
            // credential still terminal.
            "mcp_connection_failed" => (
                mcp_server_name
                    .as_ref()
                    .map_or("unknown_error", |_| "mcp_connection_failed_error"),
                RetryStatus::Retrying,
            ),
            "mcp_authentication_failed" => (
                mcp_server_name
                    .as_ref()
                    .map_or("unknown_error", |_| "mcp_authentication_failed_error"),
                RetryStatus::Terminal,
            ),
            "rate_limited" => ("model_rate_limited_error", RetryStatus::Exhausted),
            "overloaded" => ("model_overloaded_error", RetryStatus::Exhausted),
            // These are model-owned, non-retryable failures. In particular,
            // billing exhaustion must not lose its model origin or look like
            // ordinary retry exhaustion at the Managed boundary.
            "binding_rejected" | "context_overflow" | "invalid_request" | "unauthorized"
            | "usage_limit" | "login_required" | "model_not_found" | "content_filtered" => {
                ("model_request_failed_error", RetryStatus::Terminal)
            }
            "provider_error" | "timeout" => ("model_request_failed_error", RetryStatus::Exhausted),
            "activation_failed" => ("unknown_error", RetryStatus::Terminal),
            _ => ("unknown_error", RetryStatus::Exhausted),
        };
        Self {
            kind,
            message,
            retry_status,
            mcp_server_name,
        }
    }
}

fn code_message_server_name(code: &str, message: &str) -> Option<String> {
    if !code.starts_with("mcp_") {
        return None;
    }
    let marker = "mcp server `";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    Some(rest.split('`').next()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Causes: the committed neutral code and, for MCP faults, presence of the
    /// server marker in the message. Effects: the SDK kind, required server
    /// field and recovery status are selected together. Missing MCP identity
    /// cannot claim an MCP variant; distinct model rate, overload and hard
    /// usage causes cannot collapse into one wire kind.
    ///
    /// | Rule | code                      | marker in message | kind                           | mcp_server_name | retry_status |
    /// | R1   | mcp_authentication_failed | yes               | mcp_authentication_failed_error | the name        | terminal     |
    /// | R2   | mcp_authentication_failed | no                | unknown_error                   | absent          | terminal     |
    /// | R3   | mcp_connection_failed     | yes               | mcp_connection_failed_error     | the name        | retrying     |
    /// | R4   | mcp_connection_failed     | no                | unknown_error                   | absent          | retrying     |
    /// | R5   | rate_limited              | irrelevant        | model_rate_limited_error        | absent          | exhausted    |
    /// | R6   | overloaded                | irrelevant        | model_overloaded_error          | absent          | exhausted    |
    /// | R7   | usage_limit               | irrelevant        | model_request_failed_error      | absent          | terminal     |
    /// | R8   | provider_error            | irrelevant        | model_request_failed_error      | absent          | exhausted    |
    /// | R9   | timeout                   | irrelevant        | model_request_failed_error      | absent          | exhausted    |
    /// | R10  | binding_rejected          | irrelevant        | model_request_failed_error      | absent          | terminal     |
    /// | R11  | invalid_request           | irrelevant        | model_request_failed_error      | absent          | terminal     |
    /// | R12  | model_not_found           | irrelevant        | model_request_failed_error      | absent          | terminal     |
    /// | R13  | content_filtered          | irrelevant        | model_request_failed_error      | absent          | terminal     |
    /// R8/R9 a classified, exhausted model request preserves its cause and
    /// stays usable; it is not the separate ExecutionUnknown/terminal decision.
    /// R10-R13 known permanent model refusals stay terminal, never retry-exhausted.
    /// R14 unrecognized faults retain the existing generic fallback.
    /// R5-R7 distinguish rate, overload and hard usage causes at the existing
    /// classifier. All three have no MCP server field; a terminal Run with an
    /// exhausted retry budget must not be advertised as retrying.
    #[test]
    fn classified_faults_select_only_sdk_variants_their_fields_can_satisfy() {
        let cases = [
            (
                "R1",
                "mcp_authentication_failed",
                "mcp server `calc`: HTTP 401",
                "mcp_authentication_failed_error",
                Some("calc"),
                RetryStatus::Terminal,
            ),
            (
                "R2",
                "mcp_authentication_failed",
                "the upstream rejected the credential",
                "unknown_error",
                None,
                RetryStatus::Terminal,
            ),
            (
                "R3",
                "mcp_connection_failed",
                "mcp server `offline`: connection refused",
                "mcp_connection_failed_error",
                Some("offline"),
                RetryStatus::Retrying,
            ),
            (
                "R4",
                "mcp_connection_failed",
                "connection refused",
                "unknown_error",
                None,
                RetryStatus::Retrying,
            ),
            (
                "R5",
                "rate_limited",
                "mcp server `calc` is irrelevant here",
                "model_rate_limited_error",
                None,
                RetryStatus::Exhausted,
            ),
            (
                "R6",
                "overloaded",
                "provider capacity exhausted",
                "model_overloaded_error",
                None,
                RetryStatus::Exhausted,
            ),
            (
                "R7",
                "usage_limit",
                "hard usage limit reached",
                "model_request_failed_error",
                None,
                RetryStatus::Terminal,
            ),
            (
                "R8",
                "provider_error",
                "response body failed: UnexpectedEof",
                "model_request_failed_error",
                None,
                RetryStatus::Exhausted,
            ),
            (
                "R9",
                "timeout",
                "model request deadline exceeded",
                "model_request_failed_error",
                None,
                RetryStatus::Exhausted,
            ),
            (
                "R10",
                "binding_rejected",
                "model binding is unavailable",
                "model_request_failed_error",
                None,
                RetryStatus::Terminal,
            ),
            (
                "R11",
                "invalid_request",
                "model request is malformed",
                "model_request_failed_error",
                None,
                RetryStatus::Terminal,
            ),
            (
                "R12",
                "model_not_found",
                "model no longer exists",
                "model_request_failed_error",
                None,
                RetryStatus::Terminal,
            ),
            (
                "R13",
                "content_filtered",
                "provider refused this content",
                "model_request_failed_error",
                None,
                RetryStatus::Terminal,
            ),
            (
                "R14",
                "future_unknown_fault",
                "not a classified model failure",
                "unknown_error",
                None,
                RetryStatus::Exhausted,
            ),
        ];
        for (rule, code, message, kind, server, retry_status) in cases {
            let error = SessionError::classify(code, message);
            assert_eq!(error.kind, kind, "{rule} kind");
            assert_eq!(
                error.message, message,
                "{rule} concrete cause survives projection"
            );
            assert_eq!(error.mcp_server_name.as_deref(), server, "{rule} server");
            assert_eq!(error.retry_status, retry_status, "{rule} retry_status");
            // The property every row shares, and the one the wire depends on:
            // the field exists exactly when the variant requires it. A serialized
            // event can then never omit a required property, and can never carry
            // one its variant does not declare.
            assert_eq!(
                error.kind.starts_with("mcp_"),
                error.mcp_server_name.is_some(),
                "{rule}: an mcp variant and its required field are one decision"
            );
        }
    }
}
