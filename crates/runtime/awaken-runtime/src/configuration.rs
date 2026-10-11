//! Early diagnostics for SDK composition; admission never grants capabilities.

use crate::Runtime;
use awaken_runtime_contract::diagnostic::{RuntimeDiagnostic, RuntimeErrorKind};
use awaken_runtime_contract::execution::Error;
use awaken_runtime_contract::resolved::ToolKind;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_tool_contract::tool::ToolExecutionTarget;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigurationError {
    #[error("tool `{id}` is registered more than once; register each tool ID exactly once")]
    DuplicateTool { id: String },
    #[error(
        "snapshot references tool `{id}` without an implementation; register the tool or install its plugin"
    )]
    MissingTool { id: String },
    #[error(
        "tool `{id}` requires a sandbox executor; configure RuntimeRunContext::with_tool_executor"
    )]
    MissingSandboxExecutor { id: String },
    #[error(
        "resuming an awaiting run needs a history reader; configure RuntimeRunContext::with_store or with_reader"
    )]
    MissingHistoryReader,
    #[error(
        "a committed Run outcome requires a commit coordinator; configure with_store or with_commit"
    )]
    MissingCommitCoordinator,
    #[error(
        "answering an awaiting ticket needs the store owner's clock; configure Runtime::with_clock"
    )]
    MissingClock,
    #[error(
        "no model provider configured for this runtime; configure with_llm or with_model_executor"
    )]
    MissingModelProvider,
}

impl ConfigurationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateTool { .. } => "duplicate_tool",
            Self::MissingTool { .. } => "missing_tool",
            Self::MissingSandboxExecutor { .. } => "missing_sandbox_executor",
            Self::MissingHistoryReader => "missing_history_reader",
            Self::MissingCommitCoordinator => "missing_commit_coordinator",
            Self::MissingClock => "missing_clock",
            Self::MissingModelProvider => "missing_model_provider",
        }
    }
}

impl From<ConfigurationError> for Error {
    fn from(error: ConfigurationError) -> Self {
        RuntimeDiagnostic::new(RuntimeErrorKind::InvalidConfiguration, error.code(), error).into()
    }
}

/// Validate registrations made through either the fluent or fallible API.
pub(crate) fn validate_configuration(runtime: &Runtime) -> Result<(), ConfigurationError> {
    runtime.tools.validate().map_err(|error| match error {
        awaken_tool_contract::tool::ToolError::AmbiguousTool(id) => {
            ConfigurationError::DuplicateTool { id }
        }
        _ => unreachable!("registry validation only reports ambiguous IDs"),
    })
}

/// Validate an embedded composition before starting its Run. Plugin and
/// authority validation still use the canonical resolver/execution path.
pub(crate) fn validate_for(
    runtime: &Runtime,
    snapshot: &ExecutableAgentSnapshot,
    context: &RuntimeRunContext,
) -> Result<(), Error> {
    validate_configuration(runtime)?;
    awaken_runtime_contract::resolver::RunResolver::resolve(runtime, snapshot).map_err(
        |error| {
            RuntimeDiagnostic::new(
                RuntimeErrorKind::InvalidConfiguration,
                "invalid_snapshot",
                error,
            )
        },
    )?;
    // Plugin contributions are resolved once at the canonical execution
    // seam. A preflight must not instantiate active plugins a second time.
    if snapshot.resolved_spec.plugin_ids.is_empty() && context.session_plugins.is_empty() {
        let env = runtime
            .resolve_plugin_env_with(&snapshot.resolved_spec, &context.session_plugins)
            .map_err(|error| {
                RuntimeDiagnostic::new(
                    RuntimeErrorKind::InvalidConfiguration,
                    "invalid_plugin_configuration",
                    error,
                )
            })?;
        validate_resolved_tools(runtime, &snapshot.resolved_spec, context, &env)?;
    }
    if snapshot.resolved_spec.plugin_ids.is_empty()
        && context.session_plugins.is_empty()
        && runtime.llm().is_none()
        && context.model_executor.is_none()
    {
        return Err(ConfigurationError::MissingModelProvider.into());
    }
    Ok(())
}

fn validate_resolved_tools(
    runtime: &Runtime,
    spec: &awaken_runtime_contract::resolved::ResolvedSpec,
    context: &RuntimeRunContext,
    env: &awaken_runtime_contract::plugin::ResolvedExecutionEnv,
) -> Result<(), Error> {
    for descriptor in &spec.tool_descriptors {
        if !matches!(descriptor.kind, ToolKind::Regular | ToolKind::DetachedOnly) {
            continue;
        }
        let tool = env
            .dynamic_tool(&descriptor.id)
            .or_else(|| runtime.tool(&descriptor.id).cloned())
            .ok_or_else(|| ConfigurationError::MissingTool {
                id: descriptor.id.clone(),
            })?;
        if tool.execution_target() == ToolExecutionTarget::Sandbox
            && context.tool_executor.is_none()
        {
            return Err(ConfigurationError::MissingSandboxExecutor {
                id: descriptor.id.clone(),
            }
            .into());
        }
    }
    Ok(())
}
