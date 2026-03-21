mod app;
mod context;
mod engine;
mod env;
mod handlers;
mod registry;
mod reports;

pub use app::AppRuntime;
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

pub(crate) use handlers::{
    EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc, ToolPermissionCheckerArc,
    TypedEffectAdapter, TypedScheduledActionAdapter,
};
