use std::sync::Arc;

use async_trait::async_trait;

use crate::error::StateError;
use crate::model::{EffectSpec, JsonValue, ScheduledActionSpec, decode_json};
use crate::state::{Snapshot, StateCommand};

use super::PhaseContext;

#[async_trait]
pub trait TypedScheduledActionHandler<A>: Send + Sync + 'static
where
    A: ScheduledActionSpec,
{
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        payload: A::Payload,
    ) -> Result<StateCommand, StateError>;
}

#[async_trait]
pub trait TypedEffectHandler<E>: Send + Sync + 'static
where
    E: EffectSpec,
{
    async fn handle_typed(&self, payload: E::Payload, snapshot: &Snapshot) -> Result<(), String>;
}

#[async_trait]
pub(crate) trait ErasedTypedScheduledActionHandler: Send + Sync + 'static {
    async fn handle_erased(
        &self,
        ctx: &PhaseContext,
        payload: JsonValue,
    ) -> Result<StateCommand, StateError>;
}

pub(crate) struct TypedScheduledActionAdapter<A, H> {
    pub(crate) handler: H,
    pub(crate) _marker: std::marker::PhantomData<A>,
}

#[async_trait]
impl<A, H> ErasedTypedScheduledActionHandler for TypedScheduledActionAdapter<A, H>
where
    A: ScheduledActionSpec,
    H: TypedScheduledActionHandler<A>,
{
    async fn handle_erased(
        &self,
        ctx: &PhaseContext,
        payload: JsonValue,
    ) -> Result<StateCommand, StateError> {
        self.handler
            .handle_typed(ctx, A::decode_payload(payload)?)
            .await
    }
}

#[async_trait]
pub(crate) trait ErasedTypedEffectHandler: Send + Sync + 'static {
    async fn handle_erased(&self, payload: JsonValue, snapshot: &Snapshot) -> Result<(), String>;
}

pub(crate) struct TypedEffectAdapter<E, H> {
    pub(crate) handler: H,
    pub(crate) _marker: std::marker::PhantomData<E>,
}

#[async_trait]
impl<E, H> ErasedTypedEffectHandler for TypedEffectAdapter<E, H>
where
    E: EffectSpec,
    H: TypedEffectHandler<E>,
{
    async fn handle_erased(&self, payload: JsonValue, snapshot: &Snapshot) -> Result<(), String> {
        let payload = decode_json::<E::Payload>(E::KEY, payload).map_err(|err| err.to_string())?;
        self.handler.handle_typed(payload, snapshot).await
    }
}

pub(crate) type ScheduledActionHandlerArc = Arc<dyn ErasedTypedScheduledActionHandler>;
pub(crate) type EffectHandlerArc = Arc<dyn ErasedTypedEffectHandler>;

#[async_trait]
pub trait PhaseHook: Send + Sync + 'static {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError>;
}

pub(crate) type PhaseHookArc = Arc<dyn PhaseHook>;

/// Tool call permission decision.
///
/// Aggregation rule (all checkers run, then aggregate):
/// 1. Any `Deny` → final Deny (highest priority)
/// 2. Any `Allow` (no Deny) → final Allow
/// 3. All `Abstain` → final Suspend (no checker approved the call)
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPermission {
    /// Explicitly allow the tool call.
    Allow,
    /// Deny the tool call (cannot be overridden).
    Deny { reason: String },
    /// No opinion on this tool call.
    Abstain,
}

impl ToolPermission {
    pub fn is_allow(&self) -> bool {
        matches!(self, ToolPermission::Allow)
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, ToolPermission::Deny { .. })
    }

    pub fn is_abstain(&self) -> bool {
        matches!(self, ToolPermission::Abstain)
    }
}

/// Aggregated result of all tool permission checks.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPermissionResult {
    /// Tool call is allowed.
    Allow,
    /// Tool call is denied.
    Deny { reason: String },
    /// No checker allowed the call — tool is suspended pending external approval.
    Suspend,
}

impl ToolPermissionResult {
    pub fn is_allow(&self) -> bool {
        matches!(self, ToolPermissionResult::Allow)
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, ToolPermissionResult::Deny { .. })
    }

    pub fn is_suspend(&self) -> bool {
        matches!(self, ToolPermissionResult::Suspend)
    }
}

/// Aggregate individual tool permission decisions into a final result.
///
/// Priority: any Deny → Deny; any Allow (no Deny) → Allow; all Abstain → Suspend.
pub fn aggregate_tool_permissions(decisions: &[ToolPermission]) -> ToolPermissionResult {
    let mut has_allow = false;
    for decision in decisions {
        match decision {
            ToolPermission::Deny { reason } => {
                return ToolPermissionResult::Deny {
                    reason: reason.clone(),
                };
            }
            ToolPermission::Allow => {
                has_allow = true;
            }
            ToolPermission::Abstain => {}
        }
    }
    if has_allow {
        ToolPermissionResult::Allow
    } else {
        ToolPermissionResult::Suspend
    }
}

/// Plugin-customizable tool call permission check.
///
/// Registered via `PluginRegistrar::register_tool_permission`. All checkers
/// run for every tool call, and results are aggregated:
/// - Any `Deny` → tool call blocked
/// - Any `Allow` (no Deny) → tool call proceeds
/// - All `Abstain` → tool call suspended (needs external approval)
#[async_trait]
pub trait ToolPermissionChecker: Send + Sync + 'static {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError>;
}

pub(crate) type ToolPermissionCheckerArc = Arc<dyn ToolPermissionChecker>;
