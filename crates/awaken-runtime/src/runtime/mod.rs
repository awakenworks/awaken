mod agent_runtime;
mod app;
mod cancellation;
mod context;
mod engine;
mod env;
mod handlers;
mod registry;
mod reports;
mod resolver;

pub use agent_runtime::{AgentRuntime, RunRequest};
pub use app::AppRuntime;
pub use cancellation::CancellationToken;
pub use context::PhaseContext;
pub use engine::PhaseRuntime;
pub use env::ExecutionEnv;
pub use handlers::{
    PhaseHook, ToolPermission, ToolPermissionChecker, ToolPermissionResult, TypedEffectHandler,
    TypedScheduledActionHandler, aggregate_tool_permissions,
};
pub use reports::{
    DEFAULT_MAX_PHASE_ROUNDS, EffectDispatchReport, PhaseRunReport, SubmitCommandReport,
};
pub use resolver::{AgentResolver, ResolvedAgent};

pub(crate) use handlers::{
    EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc, ToolPermissionCheckerArc,
    TypedEffectAdapter, TypedScheduledActionAdapter,
};
