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
    pub(crate) fn from_run_state(
        state: &awaken_agent_contract::agent::run::RunState,
    ) -> Option<Self> {
        use awaken_agent_contract::agent::run::{EndCause, RunState};
        match state {
            RunState::Ended(EndCause::Error(failure)) => {
                Some(Self::classify(failure.code(), failure.message()))
            }
            RunState::Ended(EndCause::Indeterminate) => Some(Self::execution_unknown()),
            _ => None,
        }
    }

    /// One root/child classifier over the same lifecycle and recovery prefix.
    /// Cancellation intent never supplies physical termination evidence; later
    /// receipts cannot rewrite an already published historical diagnosis.
    pub(crate) fn from_committed_lifecycle(
        terminal: &awaken_agent_contract::RunLifecycleEvent,
        snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Option<Self>, awaken_agent_contract::agent::state::StateError> {
        use awaken_agent_contract::agent::run::{EndCause, RunState};
        if matches!(terminal.state, RunState::Ended(EndCause::Cancelled))
            && let Some(snapshot) = snapshot
        {
            let operations =
                awaken_runtime_contract::model_operation::ActiveModelOperation::recorded_at_commit(
                    &snapshot.state,
                    &snapshot.state_commit_cursors,
                    &terminal.run_id,
                    terminal.source_commit_cursor,
                )?;
            if operations
                .iter()
                .any(|operation| !operation.execution.permits_reentry())
            {
                return Ok(Some(Self::execution_unknown()));
            }
        }
        Ok(Self::from_run_state(&terminal.state))
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
            "mcp_connection_failed" => ("mcp_connection_failed_error", RetryStatus::Retrying),
            "mcp_authentication_failed" => {
                ("mcp_authentication_failed_error", RetryStatus::Terminal)
            }
            "rate_limited" => ("model_rate_limited_error", RetryStatus::Exhausted),
            // These are model-owned, non-retryable failures. In particular,
            // billing exhaustion must not lose its model origin or look like
            // ordinary retry exhaustion at the Managed boundary.
            "context_overflow" | "unauthorized" | "usage_limit" | "login_required" => {
                ("model_request_failed_error", RetryStatus::Terminal)
            }
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
