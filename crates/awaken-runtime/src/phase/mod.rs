mod context;
mod engine;
mod env;
mod handlers;
mod hooks;
mod permission;
mod queue_plugin;
mod reports;

pub use context::PhaseContext;
pub use engine::PhaseRuntime;
pub use env::ExecutionEnv;
pub use handlers::{TypedEffectHandler, TypedScheduledActionHandler};
pub use hooks::PhaseHook;
pub use permission::{
    ToolPermission, ToolPermissionChecker, ToolPermissionResult, aggregate_tool_permissions,
};
pub use reports::{
    DEFAULT_MAX_PHASE_ROUNDS, EffectDispatchReport, PhaseRunReport, SubmitCommandReport,
};

pub(crate) use handlers::{
    EffectHandlerArc, ScheduledActionHandlerArc, TypedEffectAdapter, TypedScheduledActionAdapter,
};
pub(crate) use hooks::PhaseHookArc;
pub(crate) use permission::ToolPermissionCheckerArc;
