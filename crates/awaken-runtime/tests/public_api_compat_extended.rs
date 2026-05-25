//! Extended 0.2 public-API compat lockdown.
//!
//! Wider than `public_api_compat.rs`: captures function-pointer signatures for
//! the concrete public methods 0.2 users called, and exhausts every variant of
//! every 0.2 public enum. Any SemVer-breaking change — signature drift, removed
//! variant, renamed field, changed visibility — fails this file at compile time.

#![allow(dead_code, clippy::type_complexity)]

use std::sync::Arc;

use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::profile_store::ProfileStore;
use awaken_contract::contract::storage::{RunRequestOrigin, ThreadRunStore};
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_runtime::backend::{
    BackendCapabilities, BackendControl, BackendLocalRootContext, BackendRootRunRequest,
    ExecutionBackend, ExecutionBackendError, LocalBackend,
};
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_runtime::registry::{
    AgentResolver, ExecutionResolver, RegistryHandle, RegistrySet, RegistrySnapshot, ResolvedAgent,
    ResolvedExecution,
};
use awaken_runtime::{AgentRuntime, RunRequest, RuntimeError};

struct NopResolver;

impl AgentResolver for NopResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        unreachable!("compat test does not execute")
    }
}

impl ExecutionResolver for NopResolver {
    fn resolve_execution(&self, _agent_id: &str) -> Result<ResolvedExecution, RuntimeError> {
        unreachable!("compat test does not execute")
    }
}

// Force-resolve every 0.2 AgentRuntime method we promise. Any signature drift
// turns this coercion into a compile error.
#[test]
fn agent_runtime_0_2_methods_resolve_at_expected_signatures() {
    let _new: fn(Arc<dyn AgentResolver>) -> AgentRuntime = AgentRuntime::new;
    let _new_exec: fn(Arc<dyn ExecutionResolver>) -> AgentRuntime =
        AgentRuntime::new_with_execution_resolver;
    let _with_reg: fn(AgentRuntime, RegistryHandle) -> AgentRuntime =
        AgentRuntime::with_registry_handle;
    let _with_store: fn(AgentRuntime, Arc<dyn ThreadRunStore>) -> AgentRuntime =
        AgentRuntime::with_thread_run_store;
    let _resolver: fn(&AgentRuntime) -> &dyn AgentResolver = AgentRuntime::resolver;
    let _resolver_arc: fn(&AgentRuntime) -> Arc<dyn AgentResolver> = AgentRuntime::resolver_arc;
    let _exec_resolver: fn(&AgentRuntime) -> &dyn ExecutionResolver =
        AgentRuntime::execution_resolver;
    let _exec_resolver_arc: fn(&AgentRuntime) -> Arc<dyn ExecutionResolver> =
        AgentRuntime::execution_resolver_arc;
    let _reg_handle: fn(&AgentRuntime) -> Option<RegistryHandle> = AgentRuntime::registry_handle;
    let _reg_snap: fn(&AgentRuntime) -> Option<RegistrySnapshot> = AgentRuntime::registry_snapshot;
    let _reg_ver: fn(&AgentRuntime) -> Option<u64> = AgentRuntime::registry_version;
    let _reg_set: fn(&AgentRuntime) -> Option<RegistrySet> = AgentRuntime::registry_set;
    let _replace_set: fn(&AgentRuntime, RegistrySet) -> Option<u64> =
        AgentRuntime::replace_registry_set;
    let _thread_store: fn(&AgentRuntime) -> Option<&dyn ThreadRunStore> =
        AgentRuntime::thread_run_store;
}

#[test]
fn local_backend_0_2_surface_intact() {
    let backend: Arc<dyn ExecutionBackend> = Arc::new(LocalBackend::new());
    let caps: BackendCapabilities = backend.capabilities();
    // 0.2 provided `BackendCapabilities::full`.
    let _full = BackendCapabilities::full();
    // Compile-only: confirm the eight public fields are still accessible.
    let _ = (
        caps.cancellation,
        caps.decisions,
        caps.overrides,
        caps.frontend_tools,
        caps.continuation,
        caps.waits,
        caps.transcript,
        caps.output,
    );
}

#[test]
fn run_origin_exhaustive_match_keeps_0_2_variants() {
    fn label(origin: RunOrigin) -> &'static str {
        match origin {
            RunOrigin::User => "user",
            RunOrigin::Subagent => "subagent",
            RunOrigin::Internal => "internal",
            RunOrigin::Mcp => "mcp",
        }
    }
    assert_eq!(label(RunOrigin::User), "user");
    assert_eq!(label(RunOrigin::Subagent), "subagent");
    assert_eq!(label(RunOrigin::Internal), "internal");
    assert_eq!(label(RunOrigin::Mcp), "mcp");
}

#[test]
fn run_request_origin_exhaustive_match_keeps_0_2_variants() {
    fn label(o: RunRequestOrigin) -> &'static str {
        match o {
            RunRequestOrigin::User => "user",
            RunRequestOrigin::A2A => "a2a",
            RunRequestOrigin::Internal => "internal",
            RunRequestOrigin::Mcp => "mcp",
        }
    }
    assert_eq!(label(RunRequestOrigin::User), "user");
    assert_eq!(label(RunRequestOrigin::A2A), "a2a");
    assert_eq!(label(RunRequestOrigin::Internal), "internal");
    assert_eq!(label(RunRequestOrigin::Mcp), "mcp");
}

#[test]
fn run_mode_exhaustive_match_keeps_0_2_variants() {
    fn label(mode: RunMode) -> &'static str {
        match mode {
            RunMode::Foreground => "fg",
            RunMode::Scheduled => "sched",
            RunMode::Resume => "resume",
            RunMode::InternalWake => "wake",
        }
    }
    assert_eq!(label(RunMode::Foreground), "fg");
    assert_eq!(label(RunMode::Scheduled), "sched");
    assert_eq!(label(RunMode::Resume), "resume");
    assert_eq!(label(RunMode::InternalWake), "wake");
}

#[test]
fn adapter_kind_exhaustive_match_keeps_0_2_variants() {
    fn label(kind: AdapterKind) -> &'static str {
        match kind {
            AdapterKind::Internal => "internal",
            AdapterKind::Acp => "acp",
            AdapterKind::AiSdk => "ai_sdk",
            AdapterKind::AgUi => "ag_ui",
            AdapterKind::A2a => "a2a",
            AdapterKind::Mcp => "mcp",
        }
    }
    assert_eq!(label(AdapterKind::Internal), "internal");
    assert_eq!(label(AdapterKind::Acp), "acp");
    assert_eq!(label(AdapterKind::AiSdk), "ai_sdk");
    assert_eq!(label(AdapterKind::AgUi), "ag_ui");
    assert_eq!(label(AdapterKind::A2a), "a2a");
    assert_eq!(label(AdapterKind::Mcp), "mcp");
}

#[test]
fn runtime_error_keeps_0_2_variants() {
    let _ = RuntimeError::ThreadAlreadyRunning {
        thread_id: "t".into(),
    };
    let _ = RuntimeError::AgentNotFound {
        agent_id: "a".into(),
    };
    let _ = RuntimeError::ResolveFailed {
        message: "m".into(),
    };
}

// BackendRootRunRequest must still accept 0.2 field set without ThreadContextSnapshot.
#[test]
fn backend_root_run_request_accepts_0_2_literal_without_thread_ctx() {
    let resolver = NopResolver;
    let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);
    let _req = BackendRootRunRequest {
        agent_id: "a",
        messages: Vec::<Message>::new(),
        new_messages: Vec::<Message>::new(),
        sink,
        resolver: &resolver,
        run_identity: RunIdentity::new(
            "t".to_string(),
            None,
            "r".to_string(),
            None,
            "a".to_string(),
            RunOrigin::User,
        ),
        checkpoint_store: None,
        control: BackendControl::default(),
        decisions: Vec::new(),
        overrides: None,
        frontend_tools: Vec::new(),
        local: Option::<BackendLocalRootContext<'_>>::None,
        inbox: None,
        is_continuation: false,
    };
}

// AgentRuntime::run and AgentRuntime::cancel must keep 0.2 callable shapes.
#[test]
fn agent_runtime_run_cancel_signatures_intact() {
    let _run: for<'a> fn(
        &'a AgentRuntime,
        RunRequest,
        Arc<dyn EventSink>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AgentRunResult, AgentLoopError>> + Send + 'a>,
    > = |rt, req, sink| Box::pin(rt.run(req, sink));
    let _cancel: fn(&AgentRuntime, &str) -> bool = AgentRuntime::cancel;
}

// 0.2 trait-object bounds must still hold.
#[test]
fn runtime_store_bounds_are_still_object_safe_for_0_2_traits() {
    fn _accept_thread_store(_: Arc<dyn ThreadRunStore>) {}
    fn _accept_profile_store(_: Arc<dyn ProfileStore>) {}
}

#[test]
fn agent_runtime_initialize_signature_intact() {
    let _init: for<'a> fn(
        &'a AgentRuntime,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), RuntimeError>> + Send + 'a>,
    > = |rt| Box::pin(rt.initialize());
}

// ExecutionBackendError must retain its 0.2 public variants used by adapters.
#[test]
fn execution_backend_error_keeps_0_2_variants() {
    let _ = ExecutionBackendError::AgentNotFound("x".into());
    let _ = ExecutionBackendError::ExecutionFailed("x".into());
    let _ = ExecutionBackendError::RemoteError("x".into());
}
