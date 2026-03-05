use crate::runtime::run::TerminationReason;
use crate::runtime::state::AnyStateAction;
use crate::runtime::tool_call::gate::{SuspendTicket, ToolCallAction};
use crate::runtime::tool_call::ToolResult;

/// A typed collection of actions for a specific phase.
///
/// `ActionSet<A>` is the return type of all [`AgentBehavior`](super::super::behavior::AgentBehavior)
/// hooks. It is the unit of composition: plugins can define named functions
/// that return `ActionSet<A>` combining multiple core actions, and callers
/// compose them with [`ActionSet::and`].
///
/// [`From<A> for ActionSet<A>`] allows a single action to be returned anywhere
/// an `ActionSet<A>` is expected, and [`From<AnyStateAction> for A`] is
/// implemented for every phase action enum so state changes can be expressed
/// without explicit wrapping.
#[derive(Default)]
pub struct ActionSet<A>(Vec<A>);

impl<A> ActionSet<A> {
    /// Empty set — default value, returned when a plugin does nothing.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Single-action set.
    pub fn single(a: impl Into<A>) -> Self {
        Self(vec![a.into()])
    }

    /// Combine with another action set or anything that converts into one.
    #[must_use]
    pub fn and(mut self, other: impl Into<ActionSet<A>>) -> Self {
        self.0.extend(other.into().0);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Borrow the inner slice.
    pub fn as_slice(&self) -> &[A] {
        &self.0
    }

    /// Consume into the inner `Vec`.
    pub fn into_vec(self) -> Vec<A> {
        self.0
    }
}

impl<A> IntoIterator for ActionSet<A> {
    type Item = A;
    type IntoIter = std::vec::IntoIter<A>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<A> From<Vec<A>> for ActionSet<A> {
    fn from(v: Vec<A>) -> Self {
        Self(v)
    }
}

impl<A> Extend<A> for ActionSet<A> {
    fn extend<T: IntoIterator<Item = A>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

// =========================================================================
// Phase-specific action enums
// =========================================================================

/// Actions valid in lifecycle phases: RunStart, StepStart, StepEnd, RunEnd.
///
/// Only state changes are valid here; there is no inference or tool context.
pub enum LifecycleAction {
    State(AnyStateAction),
}

impl From<AnyStateAction> for LifecycleAction {
    fn from(sa: AnyStateAction) -> Self {
        Self::State(sa)
    }
}

impl From<LifecycleAction> for ActionSet<LifecycleAction> {
    fn from(a: LifecycleAction) -> Self {
        ActionSet::single(a)
    }
}

impl From<AnyStateAction> for ActionSet<LifecycleAction> {
    fn from(sa: AnyStateAction) -> Self {
        ActionSet::single(LifecycleAction::State(sa))
    }
}

// -------------------------------------------------------------------------

/// Actions valid in `BeforeInference`.
pub enum BeforeInferenceAction {
    /// Append a system-prompt context block.
    AddSystemContext(String),
    /// Append a session message.
    AddSessionContext(String),
    /// Remove one tool by id.
    ExcludeTool(String),
    /// Keep only the listed tool ids.
    IncludeOnlyTools(Vec<String>),
    /// Request run termination before inference fires.
    Terminate(TerminationReason),
    /// Emit a persistent state change.
    State(AnyStateAction),
}

impl From<AnyStateAction> for BeforeInferenceAction {
    fn from(sa: AnyStateAction) -> Self {
        Self::State(sa)
    }
}

impl From<BeforeInferenceAction> for ActionSet<BeforeInferenceAction> {
    fn from(a: BeforeInferenceAction) -> Self {
        ActionSet::single(a)
    }
}

impl From<AnyStateAction> for ActionSet<BeforeInferenceAction> {
    fn from(sa: AnyStateAction) -> Self {
        ActionSet::single(BeforeInferenceAction::State(sa))
    }
}

// -------------------------------------------------------------------------

/// Actions valid in `AfterInference`.
pub enum AfterInferenceAction {
    /// Request run termination after seeing the LLM response.
    Terminate(TerminationReason),
    /// Emit a persistent state change.
    State(AnyStateAction),
}

impl From<AnyStateAction> for AfterInferenceAction {
    fn from(sa: AnyStateAction) -> Self {
        Self::State(sa)
    }
}

impl From<AfterInferenceAction> for ActionSet<AfterInferenceAction> {
    fn from(a: AfterInferenceAction) -> Self {
        ActionSet::single(a)
    }
}

impl From<AnyStateAction> for ActionSet<AfterInferenceAction> {
    fn from(sa: AnyStateAction) -> Self {
        ActionSet::single(AfterInferenceAction::State(sa))
    }
}

// -------------------------------------------------------------------------

/// Actions valid in `BeforeToolExecute`.
pub enum BeforeToolExecuteAction {
    /// Block tool execution with a denial reason.
    Block(String),
    /// Suspend tool execution pending external confirmation.
    Suspend(SuspendTicket),
    /// Short-circuit tool execution with a pre-built result.
    SetToolResult(ToolResult),
    /// Emit a persistent state change.
    State(AnyStateAction),
}

impl BeforeToolExecuteAction {
    /// Convenience: forward a [`ToolCallAction`] as a `BeforeToolExecuteAction`.
    pub fn from_decision(decision: ToolCallAction) -> Self {
        match decision {
            ToolCallAction::Block { reason } => Self::Block(reason),
            ToolCallAction::Suspend(ticket) => Self::Suspend(*ticket),
            ToolCallAction::Proceed => {
                unreachable!("Proceed is not emitted as a BeforeToolExecuteAction")
            }
        }
    }
}

impl From<AnyStateAction> for BeforeToolExecuteAction {
    fn from(sa: AnyStateAction) -> Self {
        Self::State(sa)
    }
}

impl From<BeforeToolExecuteAction> for ActionSet<BeforeToolExecuteAction> {
    fn from(a: BeforeToolExecuteAction) -> Self {
        ActionSet::single(a)
    }
}

impl From<AnyStateAction> for ActionSet<BeforeToolExecuteAction> {
    fn from(sa: AnyStateAction) -> Self {
        ActionSet::single(BeforeToolExecuteAction::State(sa))
    }
}

// -------------------------------------------------------------------------

/// Actions valid in `AfterToolExecute`.
pub enum AfterToolExecuteAction {
    /// Append a system-role reminder after the tool result.
    AddSystemReminder(String),
    /// Append a user-role message after the tool result.
    AddUserMessage(String),
    /// Emit a persistent state change.
    State(AnyStateAction),
}

impl From<AnyStateAction> for AfterToolExecuteAction {
    fn from(sa: AnyStateAction) -> Self {
        Self::State(sa)
    }
}

impl From<AfterToolExecuteAction> for ActionSet<AfterToolExecuteAction> {
    fn from(a: AfterToolExecuteAction) -> Self {
        ActionSet::single(a)
    }
}

impl From<AnyStateAction> for ActionSet<AfterToolExecuteAction> {
    fn from(sa: AnyStateAction) -> Self {
        ActionSet::single(AfterToolExecuteAction::State(sa))
    }
}
