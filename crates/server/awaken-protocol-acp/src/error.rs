//! ACP turn-failure classification (ported from oversight-next's
//! `acp_event_source::failure_classification`, ADR-0056/0089 recognition).
//!
//! One responsibility: turn a raw ACP outcome — a structured transport error, a
//! wall-clock deadline, a refusal, or a provider quota streamed as assistant TEXT
//! — into a neutral [`AcpFailure`] the runtime surfaces as a terminal outcome
//! plus a human-readable [`AcpFailure::prompt`]. Provider-specific recognition is
//! isolated here (G12).
//!
//! **Scope note (deliberate divergence from upstream):** oversight uses this
//! taxonomy to drive credential *cooldown*, bounded stream *reconnect/resume*,
//! and budget-exempt *re-dispatch*. This port keeps only the **classification and
//! the error prompt** — it performs no rescheduling and no retry. Each class maps
//! to a terminal [`TerminationReason`]; recovery, if any, is a host policy that
//! lives above this crate, not here.

use crate::TerminationReason;

/// A raw ACP transport error reduced to the fields classification needs. Kept
/// neutral (no `agent-client-protocol` types) so it classifies the newline-JSON
/// stand-in and the official codec alike; the `real-acp` feature adds a
/// conversion from `agent_client_protocol::Error`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawAcpError {
    /// The provider's rendered error message.
    pub message: String,
    /// Structured `errorKind` / `error_kind`, lower-cased by the caller or not.
    pub kind: Option<String>,
    /// JSON-RPC code, when the transport supplied one.
    pub code: Option<i32>,
    /// A provider-supplied retry/reset hint in seconds, when named.
    pub retry_after_secs: Option<u64>,
}

impl RawAcpError {
    /// A raw error from just a message (the common newline-JSON case).
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            ..Default::default()
        }
    }

    fn kind_is(&self, needle: &str) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|k| k.eq_ignore_ascii_case(needle))
    }

    fn haystack(&self) -> String {
        let mut s = self.message.to_ascii_lowercase();
        if let Some(k) = &self.kind {
            s.push(' ');
            s.push_str(&k.to_ascii_lowercase());
        }
        s
    }
}

/// The stage of the ACP session a failure occurred at. A connection drop at
/// `Prompt` is the backend cutting an in-flight turn (external unavailability); the
/// same drop at `Initialize`/`NewSession` is a launch/config fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Initialize,
    NewSession,
    Prompt,
}

/// Which credential-death sub-kind a [`AcpFailureClass::CredentialRejected`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// Org/account subscription disabled for this CLI (`oauth_org_not_allowed`).
    SubscriptionDisabled,
    /// The operator must re-authenticate the CLI (login/OAuth expired).
    LoginRequired,
    /// The provider rejected the credential (401/403, invalid/expired key).
    AuthenticationError,
}

impl CredentialKind {
    /// The `auth_failure:<kind>` code oversight quarantines on (kept for parity /
    /// observability; this crate does not act on it).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::SubscriptionDisabled => "auth_failure:subscription_disabled",
            Self::LoginRequired => "auth_failure:login_required",
            Self::AuthenticationError => "auth_failure:authentication_error",
        }
    }
}

/// The neutral failure class of an ACP turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpFailureClass {
    /// A windowed-quota exhaustion or in-flight backend drop — the source is
    /// unusable now. `retry_after_secs` is the provider's reset hint, when named.
    RateLimited { retry_after_secs: Option<u64> },
    /// A permanent credential death that stays broken until an operator acts.
    CredentialRejected { kind: CredentialKind },
    /// A transient stream/transport interruption (429/overloaded/timeout/5xx).
    Transient,
    /// The agent refused the request (policy/guardrail).
    Refusal,
    /// The turn exceeded its wall-clock deadline and was reaped.
    Timeout,
    /// A permanent runtime failure with no more specific class.
    Permanent,
}

/// A classified ACP turn failure: the neutral class plus the raw provider message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpFailure {
    pub class: AcpFailureClass,
    /// The raw provider/adapter message, retained for logs and the prompt tail.
    pub message: String,
}

impl AcpFailure {
    fn new(class: AcpFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    /// The terminal [`TerminationReason`] this failure maps to. Rate-limit,
    /// credential, transient, and permanent are all `Error` terminations here —
    /// this crate does not retry them; a host may, above this seam.
    #[must_use]
    pub fn termination(&self) -> TerminationReason {
        match self.class {
            AcpFailureClass::Refusal => TerminationReason::Refusal,
            AcpFailureClass::Timeout => TerminationReason::TimedOut,
            _ => TerminationReason::Error,
        }
    }

    /// A human-readable error prompt for the run's surface. Explains what happened
    /// and — deliberately — that the run stopped without retrying (this crate does
    /// no rescheduling; recovery is a host concern).
    #[must_use]
    pub fn prompt(&self) -> String {
        let tail = if self.message.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.message)
        };
        match self.class {
            AcpFailureClass::RateLimited { retry_after_secs } => {
                let when = match retry_after_secs {
                    Some(s) => format!(" retry after ~{s}s"),
                    None => String::new(),
                };
                format!(
                    "The model provider's quota is exhausted or the connection was \
                     cut mid-turn;{when} the run stopped without retrying.{tail}"
                )
            }
            AcpFailureClass::CredentialRejected { kind } => match kind {
                CredentialKind::SubscriptionDisabled => format!(
                    "The provider account is disabled for this agent CLI; an operator \
                     must re-enable it or switch credentials.{tail}"
                ),
                CredentialKind::LoginRequired => format!(
                    "The agent CLI needs re-authentication (login/OAuth expired); an \
                     operator must log it in again.{tail}"
                ),
                CredentialKind::AuthenticationError => {
                    format!("The provider rejected the credential (invalid or expired key).{tail}")
                }
            },
            AcpFailureClass::Transient => {
                format!("A transient provider/stream interruption ended the turn.{tail}")
            }
            AcpFailureClass::Refusal => {
                format!("The agent refused the request (policy/guardrail).{tail}")
            }
            AcpFailureClass::Timeout => {
                format!("The turn exceeded its wall-clock deadline and was reaped.{tail}")
            }
            AcpFailureClass::Permanent => format!("The agent turn failed.{tail}"),
        }
    }
}

/// A run wall-clock deadline was exceeded (reaped). Timeout class.
#[must_use]
pub fn deadline_exceeded(deadline_secs: u64) -> AcpFailure {
    AcpFailure::new(
        AcpFailureClass::Timeout,
        format!("ACP turn exceeded the {deadline_secs}s wall-clock deadline — failing closed"),
    )
}

/// The agent refused (policy/guardrail). Refusal class.
#[must_use]
pub fn refusal(message: impl Into<String>) -> AcpFailure {
    AcpFailure::new(AcpFailureClass::Refusal, message.into())
}

/// Classify a raw ACP transport error at `stage` into a neutral [`AcpFailure`].
/// Mirrors oversight's `acp_failure`, in the same precedence order: org-disable →
/// auth-rejection → provider quota → in-flight drop → transient → permanent.
#[must_use]
pub fn classify_error(stage: Stage, err: &RawAcpError) -> AcpFailure {
    if is_org_subscription_disabled(err) {
        return AcpFailure::new(
            AcpFailureClass::CredentialRejected {
                kind: CredentialKind::SubscriptionDisabled,
            },
            err.message.clone(),
        );
    }
    if let Some(kind) = auth_required_kind(err) {
        return AcpFailure::new(
            AcpFailureClass::CredentialRejected { kind },
            err.message.clone(),
        );
    }
    if let Some(retry_after_secs) = provider_quota_retry(err) {
        return AcpFailure::new(
            AcpFailureClass::RateLimited { retry_after_secs },
            err.message.clone(),
        );
    }
    if stage == Stage::Prompt && is_connection_drop(err) {
        return AcpFailure::new(
            AcpFailureClass::RateLimited {
                retry_after_secs: None,
            },
            err.message.clone(),
        );
    }
    if is_transient(err) {
        return AcpFailure::new(AcpFailureClass::Transient, err.message.clone());
    }
    AcpFailure::new(AcpFailureClass::Permanent, err.message.clone())
}

/// A HARD provider quota can arrive as assistant TEXT (`"You've hit your weekly
/// limit · resets …"`) rather than a structured error; the CLI then hangs. Detect
/// the provider's own exhaustion BANNER so the turn fails closed. Tight
/// discriminator (`"hit your … limit"`) to avoid killing an agent merely
/// *discussing* quotas. Returns a `RateLimited` failure when matched.
#[must_use]
pub fn streamed_hard_limit(assistant_text: &str) -> Option<AcpFailure> {
    let lowered = assistant_text.to_ascii_lowercase();
    let banner = lowered.contains("hit your")
        && lowered.contains("limit")
        && (lowered.contains("weekly")
            || lowered.contains("session")
            || lowered.contains("usage")
            || lowered.contains("daily"));
    banner.then(|| {
        AcpFailure::new(
            AcpFailureClass::RateLimited {
                retry_after_secs: None,
            },
            assistant_text.trim().to_string(),
        )
    })
}

// ── Recognition helpers (provider-specific, isolated here per G12) ────────────

/// JSON-RPC code Anthropic surfaces a rate/session limit through.
const ACP_RATE_LIMIT_CODE: i32 = -32603;

fn is_org_subscription_disabled(err: &RawAcpError) -> bool {
    if err.kind_is("oauth_org_not_allowed") {
        return true;
    }
    let h = err.haystack();
    h.contains("oauth_org_not_allowed") || h.contains("disabled claude subscription access")
}

fn auth_required_kind(err: &RawAcpError) -> Option<CredentialKind> {
    let h = err.haystack();
    let has = |needle: &str| err.kind_is(needle) || h.contains(needle);
    if has("login_required")
        || has("oauth_token_expired")
        || has("invalid_grant")
        || h.contains("please run /login")
        || h.contains("please log in")
        || h.contains("login required")
        || h.contains("authentication required")
        || h.contains("re-authenticate")
    {
        return Some(CredentialKind::LoginRequired);
    }
    if has("authentication_error")
        || has("permission_error")
        || has("invalid_api_key")
        || has("unauthorized")
        || has("forbidden")
        || h.contains("invalid api key")
        || h.contains("invalid x-api-key")
        || h.contains("401 unauthorized")
        || h.contains("403 forbidden")
        || h.contains("expired token")
        || h.contains("token expired")
    {
        return Some(CredentialKind::AuthenticationError);
    }
    None
}

/// A windowed-quota exhaustion (`session_limit`) — cools in oversight; here it
/// just classifies `RateLimited`. Returns `Some(retry_after_secs)` (inner `None`
/// when no reset was named). A transient `rate_limit`/`429` is NOT this — it is
/// [`is_transient`].
fn provider_quota_retry(err: &RawAcpError) -> Option<Option<u64>> {
    let kind_matches = err.kind_is("session_limit") || err.kind_is("session_limit_error");
    let msg = err.message.to_ascii_lowercase();
    let message_matches = msg.contains("session_limit") || msg.contains("session limit");
    let code_ok = err.code.is_none_or(|c| c == ACP_RATE_LIMIT_CODE);
    if code_ok && (kind_matches || message_matches) {
        return Some(err.retry_after_secs);
    }
    None
}

fn is_connection_drop(err: &RawAcpError) -> bool {
    let h = err.haystack();
    h.contains("server shut down") || h.contains("server disconnected")
}

/// Transient provider stream/transport signatures. A request-rate throttle
/// (429/`rate_limit`) is transient (bounded backoff upstream), distinct from the
/// windowed quota that cools.
fn is_transient(err: &RawAcpError) -> bool {
    const KINDS: &[&str] = &[
        "rate_limit",
        "rate_limit_error",
        "too_many_requests",
        "overloaded",
        "overloaded_error",
        "api_error",
        "timeout",
        "timeout_error",
        "stream_error",
        "network_error",
        "connection_error",
        "service_unavailable",
        "internal_server_error",
    ];
    const SIGNATURES: &[&str] = &[
        "rate limit",
        "rate_limit",
        "too many requests",
        "429",
        "stream error",
        "overloaded",
        "timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "connection error",
        "network error",
        "temporarily unavailable",
        "service unavailable",
        "502",
        "503",
        "504",
        "529",
    ];
    if let Some(kind) = &err.kind {
        let k = kind.to_ascii_lowercase();
        if KINDS.iter().any(|x| *x == k) {
            return true;
        }
    }
    let h = err.haystack();
    SIGNATURES.iter().any(|sig| h.contains(sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str) -> RawAcpError {
        RawAcpError::message(msg)
    }

    #[test]
    fn org_subscription_disable_is_a_credential_death() {
        let e = err("Your organization has disabled Claude subscription access for Claude Code");
        let f = classify_error(Stage::Prompt, &e);
        assert_eq!(
            f.class,
            AcpFailureClass::CredentialRejected {
                kind: CredentialKind::SubscriptionDisabled
            }
        );
        assert_eq!(f.termination(), TerminationReason::Error);
        assert!(f.prompt().contains("disabled"));
    }

    #[test]
    fn login_required_and_auth_error_split() {
        let login = classify_error(Stage::Prompt, &err("Please run /login to continue"));
        assert!(matches!(
            login.class,
            AcpFailureClass::CredentialRejected {
                kind: CredentialKind::LoginRequired
            }
        ));
        let auth = classify_error(Stage::Prompt, &err("401 Unauthorized: invalid api key"));
        assert!(matches!(
            auth.class,
            AcpFailureClass::CredentialRejected {
                kind: CredentialKind::AuthenticationError
            }
        ));
    }

    #[test]
    fn session_limit_is_rate_limited_not_transient() {
        let mut e = err("session_limit reached");
        e.retry_after_secs = Some(3600);
        let f = classify_error(Stage::Prompt, &e);
        assert_eq!(
            f.class,
            AcpFailureClass::RateLimited {
                retry_after_secs: Some(3600)
            }
        );
        assert!(f.prompt().contains("3600s"));
    }

    #[test]
    fn plain_429_is_transient() {
        let f = classify_error(Stage::Prompt, &err("HTTP 429 too many requests"));
        assert_eq!(f.class, AcpFailureClass::Transient);
    }

    #[test]
    fn connection_drop_at_prompt_is_rate_limited_but_at_init_is_permanent() {
        let drop = err("server disconnected unexpectedly");
        assert_eq!(
            classify_error(Stage::Prompt, &drop).class,
            AcpFailureClass::RateLimited {
                retry_after_secs: None
            }
        );
        // Same drop while starting the adapter is a launch fault → permanent.
        assert_eq!(
            classify_error(Stage::Initialize, &drop).class,
            AcpFailureClass::Permanent
        );
    }

    #[test]
    fn unrecognized_error_is_permanent() {
        let f = classify_error(Stage::Prompt, &err("segmentation fault in tool"));
        assert_eq!(f.class, AcpFailureClass::Permanent);
        assert_eq!(f.termination(), TerminationReason::Error);
    }

    #[test]
    fn streamed_weekly_limit_banner_detected_but_quota_discussion_is_not() {
        assert!(streamed_hard_limit("You've hit your weekly limit · resets Jun 30").is_some());
        // An agent merely discussing rate limits must NOT be killed.
        assert!(streamed_hard_limit("Here is how to handle a weekly rate limit in code").is_none());
    }

    fn err_kind(kind: &str) -> RawAcpError {
        let mut e = err("an opaque provider error");
        e.kind = Some(kind.to_string());
        e
    }

    #[test]
    fn transient_message_signatures_all_classify_transient() {
        for sig in [
            "overloaded",
            "request timed out",
            "connection reset by peer",
            "connection closed",
            "service unavailable",
            "a network error occurred",
            "stream error mid-turn",
            "HTTP 502 bad gateway",
            "503 service down",
            "gateway 504",
            "529 overloaded",
        ] {
            assert_eq!(
                classify_error(Stage::Prompt, &err(sig)).class,
                AcpFailureClass::Transient,
                "message `{sig}` should be transient"
            );
        }
    }

    #[test]
    fn transient_kind_signatures_all_classify_transient() {
        // The kind-based branch of `is_transient` (never exercised before).
        for kind in [
            "overloaded_error",
            "timeout",
            "timeout_error",
            "stream_error",
            "network_error",
            "service_unavailable",
            "internal_server_error",
            "api_error",
            "too_many_requests",
        ] {
            assert_eq!(
                classify_error(Stage::Prompt, &err_kind(kind)).class,
                AcpFailureClass::Transient,
                "kind `{kind}` should be transient"
            );
        }
    }

    #[test]
    fn login_required_signatures_all_map_to_login() {
        for sig in [
            "invalid_grant",
            "oauth_token_expired",
            "please log in first",
            "you must re-authenticate",
            "login required to continue",
        ] {
            assert!(
                matches!(
                    classify_error(Stage::Prompt, &err(sig)).class,
                    AcpFailureClass::CredentialRejected {
                        kind: CredentialKind::LoginRequired
                    }
                ),
                "`{sig}` should be login-required"
            );
        }
    }

    #[test]
    fn authentication_error_signatures_all_map_to_auth_error() {
        for sig in [
            "403 forbidden",
            "expired token",
            "invalid x-api-key",
            "unauthorized request",
            "permission_error",
        ] {
            assert!(
                matches!(
                    classify_error(Stage::Prompt, &err(sig)).class,
                    AcpFailureClass::CredentialRejected {
                        kind: CredentialKind::AuthenticationError
                    }
                ),
                "`{sig}` should be an authentication error"
            );
        }
    }

    #[test]
    fn server_shut_down_at_prompt_is_rate_limited() {
        assert_eq!(
            classify_error(Stage::Prompt, &err("the server shut down")).class,
            AcpFailureClass::RateLimited {
                retry_after_secs: None
            }
        );
    }

    #[test]
    fn session_limit_message_without_a_retry_is_rate_limited_with_none() {
        // Message-only quota match (no `kind`, no `retry_after`).
        let f = classify_error(Stage::Prompt, &err("session limit reached for this window"));
        assert_eq!(
            f.class,
            AcpFailureClass::RateLimited {
                retry_after_secs: None
            }
        );
    }

    #[test]
    fn each_failure_class_has_distinct_prompt_wording() {
        // The `prompt()` decision table: every class renders its own operator-facing
        // explanation, and each appends the raw message tail. Only SubscriptionDisabled
        // was covered before; the other credential kinds and the transient/timeout/
        // permanent/rate-limited-without-retry arms were not.
        let rl = AcpFailure {
            class: AcpFailureClass::RateLimited {
                retry_after_secs: None,
            },
            message: "quota gone".into(),
        };
        assert!(rl.prompt().contains("quota is exhausted"));
        assert!(rl.prompt().contains("(quota gone)"), "message tail present");
        assert!(
            !rl.prompt().contains("retry after"),
            "no retry hint without a reset: {}",
            rl.prompt()
        );

        let login = AcpFailure {
            class: AcpFailureClass::CredentialRejected {
                kind: CredentialKind::LoginRequired,
            },
            message: String::new(),
        };
        assert!(login.prompt().contains("re-authentication"));

        let auth = AcpFailure {
            class: AcpFailureClass::CredentialRejected {
                kind: CredentialKind::AuthenticationError,
            },
            message: String::new(),
        };
        assert!(auth.prompt().contains("rejected the credential"));

        let transient = AcpFailure {
            class: AcpFailureClass::Transient,
            message: String::new(),
        };
        assert!(transient.prompt().contains("transient"));

        let timeout = AcpFailure {
            class: AcpFailureClass::Timeout,
            message: String::new(),
        };
        assert!(timeout.prompt().contains("wall-clock deadline"));

        let permanent = AcpFailure {
            class: AcpFailureClass::Permanent,
            message: String::new(),
        };
        // The empty-message branch renders no ` (...)` tail.
        assert_eq!(permanent.prompt(), "The agent turn failed.");
    }

    #[test]
    fn a_session_limit_message_with_a_foreign_jsonrpc_code_is_not_rate_limited() {
        // The quota discriminator requires the JSON-RPC code to be absent or the
        // rate-limit code; a `session limit` message tagged with an unrelated code
        // must NOT classify as RateLimited (it falls through to Permanent here).
        let mut e = err("session limit reached for this window");
        e.code = Some(-32000);
        assert_eq!(
            classify_error(Stage::Prompt, &e).class,
            AcpFailureClass::Permanent,
            "a foreign code disqualifies the quota match"
        );
    }

    #[test]
    fn deadline_and_refusal_map_to_their_terminations() {
        assert_eq!(
            deadline_exceeded(120).termination(),
            TerminationReason::TimedOut
        );
        assert_eq!(refusal("blocked").termination(), TerminationReason::Refusal);
    }
}
