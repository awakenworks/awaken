mod engine;
mod env;
mod queue_plugin;
mod reports;

pub use crate::hooks::{
    PhaseContext, PhaseHook, ToolPermission, ToolPermissionChecker, ToolPermissionResult,
    TypedEffectHandler, TypedScheduledActionHandler, aggregate_tool_permissions,
};
pub use engine::PhaseRuntime;
pub use env::ExecutionEnv;
pub use reports::{
    DEFAULT_MAX_PHASE_ROUNDS, EffectDispatchReport, PhaseRunReport, SubmitCommandReport,
};

pub(crate) use crate::hooks::{
    EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc, ToolPermissionCheckerArc,
    TypedEffectAdapter, TypedScheduledActionAdapter,
};
