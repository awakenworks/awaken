use crate::runtime::run::TerminationReason;

/// Flow control extension: run-level action.
///
/// Populated by `RequestTermination` action.
#[derive(Debug, Default, Clone)]
pub struct FlowControl {
    /// Run-level action emitted by plugins.
    pub run_action: Option<RunAction>,
}

/// Run-level control action emitted by plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAction {
    /// Continue normal execution.
    Continue,
    /// Terminate run with specific reason.
    Terminate(TerminationReason),
}
