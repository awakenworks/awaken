//! Extended 0.2 public-API compat lockdown for `awaken-server`.
//!
//! Covers every public method on `Mailbox`, `RunControlService`, every variant
//! on `MailboxError` / `MailboxRunOutcome` / `InputMode` / `InterruptMode` /
//! `MailboxDispatchStatus` / `RunControlError`, plus struct-literal
//! construction for `ActiveRun` / `MailboxSubmitResult` / `MailboxConfig`.
//!
//! Also exercises the HTTP wire-compat surface by deserializing the three
//! legacy `mode` values a 0.2 client would send.

#![allow(dead_code, clippy::type_complexity)]

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::mailbox::{MailboxInterrupt, MailboxStore};
use awaken_contract::contract::storage::{RunWaitingState, ThreadRunStore};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_server::app::{MailboxLifecycleMode, ServerConfig, ShutdownConfig};
use awaken_server::mailbox::{
    Mailbox, MailboxConfig, MailboxDispatchStatus, MailboxError, MailboxRunOutcome,
    MailboxSubmitResult, RunDispatchExecutor,
};
use awaken_server::metrics;
use awaken_server::routes::ApiError;
use awaken_server::services::run_control_service::{
    ActiveRun, InputMode, InterruptMode, RunControlError,
};

// ── Signature lockdown for Mailbox ────────────────────────────────────────

struct OldExec;

#[async_trait]
impl RunDispatchExecutor for OldExec {
    async fn run(
        &self,
        _: RunActivation,
        _: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        unreachable!()
    }
    fn cancel(&self, _: &str) -> bool {
        false
    }
    async fn cancel_and_wait_by_thread(&self, _: &str) -> bool {
        false
    }
    fn send_decision(&self, _: &str, _: String, _: ToolCallResume) -> bool {
        false
    }
}

#[test]
fn mailbox_submit_signature_intact() {
    let _submit: for<'a> fn(
        &'a Arc<Mailbox>,
        RunActivation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (MailboxSubmitResult, mpsc::Receiver<AgentEvent>),
                        MailboxError,
                    >,
                > + Send
                + 'a,
        >,
    > = |mb, req| Box::pin(mb.submit(req));

    let _submit_bg: for<'a> fn(
        &'a Arc<Mailbox>,
        RunActivation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<MailboxSubmitResult, MailboxError>> + Send + 'a,
        >,
    > = |mb, req| Box::pin(mb.submit_background(req));
}

#[test]
fn mailbox_methods_signature_intact() {
    let _cancel: for<'a> fn(
        &'a Mailbox,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<bool, MailboxError>> + Send + 'a>,
    > = |mb, id| Box::pin(mb.cancel(id));

    let _interrupt: for<'a> fn(
        &'a Mailbox,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MailboxInterrupt, MailboxError>> + Send + 'a>,
    > = |mb, id| Box::pin(mb.interrupt(id));

    let _send_decision: fn(&Mailbox, &str, String, ToolCallResume) -> bool = Mailbox::send_decision;
}

#[test]
fn mailbox_interrupt_struct_literal_keeps_0_2_fields() {
    let interrupt = MailboxInterrupt {
        new_dispatch_epoch: 1,
        active_dispatch: None,
        superseded_count: 0,
    };
    assert_eq!(interrupt.new_dispatch_epoch, 1);
}

#[test]
fn mailbox_new_signature_intact() {
    // 0.6.0 collapses the previous `Mailbox::new` / `Mailbox::new_with_executor`
    // pair into a single constructor that accepts any `IntoDispatchExecutor`
    // (both `Arc<Concrete>` and `Arc<dyn RunDispatchExecutor>`). The two
    // smoke calls below cover the two prior input shapes through the new
    // unified signature.
    let mailbox_store: Arc<dyn MailboxStore> = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let run_store: Arc<dyn ThreadRunStore> = Arc::new(awaken_stores::InMemoryStore::new());
    let concrete: Arc<OldExec> = Arc::new(OldExec);
    let erased: Arc<dyn RunDispatchExecutor> = concrete.clone();

    let _ = Mailbox::new(
        concrete,
        Arc::clone(&mailbox_store),
        Arc::clone(&run_store),
        "compat".to_string(),
        MailboxConfig::default(),
    );
    let _ = Mailbox::new(
        erased,
        mailbox_store,
        run_store,
        "compat".to_string(),
        MailboxConfig::default(),
    );
}

#[test]
fn mailbox_dispatch_status_all_0_2_variants_still_exhaustive() {
    fn label(s: MailboxDispatchStatus) -> &'static str {
        match s {
            MailboxDispatchStatus::Running => "running",
            MailboxDispatchStatus::Queued => "queued",
        }
    }
    assert_eq!(label(MailboxDispatchStatus::Running), "running");
    assert_eq!(label(MailboxDispatchStatus::Queued), "queued");
}

#[test]
fn mailbox_error_keeps_0_2_variants() {
    // These three variants existed in 0.2; we accept that new ones may
    // have been added (hence the trailing `_`), but the 0.2 ones must stay.
    let _ = MailboxError::Validation("v".into());
    fn _inspect(e: &MailboxError) -> &'static str {
        match e {
            MailboxError::Validation(_) => "validation",
            MailboxError::Store(_) => "store",
            _ => "other",
        }
    }
}

#[test]
fn mailbox_run_outcome_keeps_0_2_variants() {
    let _ = MailboxRunOutcome::Completed;
    let _ = MailboxRunOutcome::TransientError("x".into());
    fn _inspect(o: &MailboxRunOutcome) -> &'static str {
        match o {
            MailboxRunOutcome::Completed => "done",
            MailboxRunOutcome::TransientError(_) => "transient",
            _ => "other",
        }
    }
}

#[test]
fn mailbox_submit_result_struct_literal_keeps_0_2_fields() {
    let r = MailboxSubmitResult {
        dispatch_id: "d".into(),
        run_id: "r".into(),
        thread_id: "t".into(),
        status: MailboxDispatchStatus::Queued,
    };
    assert_eq!(r.thread_id, "t");
}

#[test]
fn mailbox_config_default_constructs() {
    let cfg = MailboxConfig::default();
    // Fields mentioned in 0.2 docs must still exist.
    let _ = cfg.lease_ms;
    let _ = cfg.suspended_lease_ms;
}

#[test]
fn server_config_struct_literal_keeps_0_2_fields() {
    let cfg = ServerConfig {
        address: "0.0.0.0:3000".into(),
        sse_buffer_size: 64,
        replay_buffer_capacity: 1024,
        shutdown: ShutdownConfig { timeout_secs: 30 },
        max_concurrent_requests: 100,
        a2a_extended_card_bearer_token: None,
        mailbox_lifecycle: MailboxLifecycleMode::Auto,
        eval_limits: awaken_server::eval_limits::EvalLimits::default(),
    };
    assert_eq!(cfg.address, "0.0.0.0:3000");
}

#[test]
fn api_error_named_variants_keep_0_2_paths() {
    // `ApiError` is `#[non_exhaustive]`, so downstream code matching it
    // must include a wildcard arm. This test only guarantees that the
    // 0.2 named variants (and the post-0.2 additions listed below) still
    // exist with the same shape — it does NOT guarantee that an
    // exhaustive match without a wildcard arm still compiles.
    fn label(error: ApiError) -> &'static str {
        match error {
            ApiError::BadRequest(_) => "bad_request",
            ApiError::Conflict(_) => "conflict",
            ApiError::NotFound(_) => "not_found",
            ApiError::ThreadNotFound(_) => "thread_not_found",
            ApiError::RunNotFound(_) => "run_not_found",
            ApiError::ServiceUnavailable(_) => "service_unavailable",
            ApiError::Unauthorized(_) => "unauthorized",
            ApiError::Internal(_) => "internal",
            _ => "other",
        }
    }
    assert_eq!(label(ApiError::NotFound("x".into())), "not_found");
}

#[test]
fn metrics_0_2_function_signatures_still_exist() {
    let _record_run_duration: fn(f64) = metrics::record_run_duration;
    let _inc_inference_requests: fn(&str, &str) = metrics::inc_inference_requests;
    let _record_inference_duration: fn(f64) = metrics::record_inference_duration;
}

// ── Signature lockdown for RunControlService ──────────────────────────────

#[test]
fn run_control_service_0_2_surface_intact() {
    let _cancel_run: for<'a> fn(
        &'a awaken_server::services::run_control_service::RunControlService,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), RunControlError>> + Send + 'a>,
    > = |svc, id| Box::pin(svc.cancel_run(id));
}

#[test]
fn active_run_struct_literal_keeps_0_2_fields() {
    let _ = ActiveRun {
        thread_id: "t".into(),
        run_id: "r".into(),
        agent_id: "a".into(),
        status: RunStatus::Running,
        termination_reason: Option::<TerminationReason>::None,
        dispatch_id: None,
        session_id: None,
        waiting: Option::<RunWaitingState>::None,
    };
}

#[test]
fn interrupt_mode_exhaustive_match_keeps_0_2_variants() {
    fn label(m: InterruptMode) -> &'static str {
        match m {
            InterruptMode::Graceful => "graceful",
        }
    }
    assert_eq!(label(InterruptMode::Graceful), "graceful");
}

#[test]
fn run_control_error_keeps_0_2_variants() {
    let _ = RunControlError::ThreadNotFound("t".into());
    let _ = RunControlError::RunNotFound("r".into());
    let _ = RunControlError::DecisionTargetNotFound("d".into());
    // Store / Mailbox variants can't be constructed without the inner error;
    // we pattern-match instead to prove they still exist.
    fn _inspect(e: &RunControlError) -> &'static str {
        match e {
            RunControlError::ThreadNotFound(_) => "thread",
            RunControlError::RunNotFound(_) => "run",
            RunControlError::DecisionTargetNotFound(_) => "decision",
            RunControlError::Store(_) => "store",
            RunControlError::Mailbox(_) => "mailbox",
        }
    }
}

// ── HTTP wire compat — legacy JSON `mode` values still parse ──────────────
//
// The 0.2 route accepted `mode: "queue" | "interrupt_then_queue" |
// "resume_open_run"`. HEAD replaced the inner type with `PushInputMode`
// (private) but promises identical JSON acceptance. Re-prove it from the
// `InputMode` deserializer, which is still the public type users can embed
// in their own request structs.

#[test]
fn input_mode_deserializes_all_0_2_strings() {
    let q: InputMode = serde_json::from_str("\"queue\"").unwrap();
    let iq: InputMode = serde_json::from_str("\"interrupt_then_queue\"").unwrap();
    let ru: InputMode = serde_json::from_str("\"resume_open_run\"").unwrap();
    assert_eq!(q, InputMode::Queue);
    assert_eq!(iq, InputMode::InterruptThenQueue);
    assert_eq!(ru, InputMode::ResumeOpenRun);
}

#[test]
fn input_mode_default_matches_0_2() {
    assert_eq!(InputMode::default(), InputMode::Queue);
}

#[test]
fn input_mode_serialize_stays_snake_case() {
    assert_eq!(
        serde_json::to_string(&InputMode::Queue).unwrap(),
        "\"queue\""
    );
    assert_eq!(
        serde_json::to_string(&InputMode::InterruptThenQueue).unwrap(),
        "\"interrupt_then_queue\""
    );
    assert_eq!(
        serde_json::to_string(&InputMode::ResumeOpenRun).unwrap(),
        "\"resume_open_run\""
    );
}
