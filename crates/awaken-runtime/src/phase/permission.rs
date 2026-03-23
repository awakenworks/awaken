use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::StateError;

use super::PhaseContext;

/// Tool call permission decision.
///
/// Aggregation rule (all checkers run, then aggregate):
/// 1. Any `Block` → final Block (highest priority, terminates the run)
/// 2. Any `Deny` → final Deny
/// 3. Any `Allow` (no Block/Deny) → final Allow
/// 4. All `Abstain` → final Suspend (no checker approved the call)
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPermission {
    /// Explicitly allow the tool call.
    Allow,
    /// Deny the tool call with an optional custom message for the LLM.
    Deny {
        reason: String,
        /// Custom message sent back to the LLM as the tool result.
        /// When `None`, a generic "Permission denied: {reason}" message is used.
        message: Option<String>,
    },
    /// Block the tool call and terminate the entire run.
    Block { reason: String },
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

    pub fn is_block(&self) -> bool {
        matches!(self, ToolPermission::Block { .. })
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
    /// Tool call is denied; the LLM receives the custom `message` (or a generic one).
    Deny {
        reason: String,
        /// Custom message sent back to the LLM as the tool result.
        message: Option<String>,
    },
    /// Tool call is blocked and the run is terminated.
    Block { reason: String },
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

    pub fn is_block(&self) -> bool {
        matches!(self, ToolPermissionResult::Block { .. })
    }

    pub fn is_suspend(&self) -> bool {
        matches!(self, ToolPermissionResult::Suspend)
    }
}

/// Aggregate individual tool permission decisions into a final result.
///
/// Priority: Block > Deny > Allow > Suspend (all Abstain).
pub fn aggregate_tool_permissions(decisions: &[ToolPermission]) -> ToolPermissionResult {
    let mut has_allow = false;
    let mut first_deny: Option<(String, Option<String>)> = None;

    for decision in decisions {
        match decision {
            ToolPermission::Block { reason } => {
                return ToolPermissionResult::Block {
                    reason: reason.clone(),
                };
            }
            ToolPermission::Deny { reason, message } => {
                if first_deny.is_none() {
                    first_deny = Some((reason.clone(), message.clone()));
                }
            }
            ToolPermission::Allow => {
                has_allow = true;
            }
            ToolPermission::Abstain => {}
        }
    }

    if let Some((reason, message)) = first_deny {
        ToolPermissionResult::Deny { reason, message }
    } else if has_allow {
        ToolPermissionResult::Allow
    } else {
        ToolPermissionResult::Suspend
    }
}

/// Plugin-customizable tool call permission check.
///
/// Registered via `PluginRegistrar::register_tool_permission`. All checkers
/// run for every tool call, and results are aggregated:
/// - Any `Block` → tool call blocked and run terminates
/// - Any `Deny` → tool call rejected (custom message sent to LLM)
/// - Any `Allow` (no Block/Deny) → tool call proceeds
/// - All `Abstain` → tool call suspended (needs external approval)
#[async_trait]
pub trait ToolPermissionChecker: Send + Sync + 'static {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError>;
}

pub(crate) type ToolPermissionCheckerArc = Arc<dyn ToolPermissionChecker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_gate_deny_with_custom_message() {
        let decisions = vec![ToolPermission::Deny {
            reason: "unsafe".into(),
            message: Some("This tool is not allowed in sandbox mode".into()),
        }];
        let result = aggregate_tool_permissions(&decisions);
        assert_eq!(
            result,
            ToolPermissionResult::Deny {
                reason: "unsafe".into(),
                message: Some("This tool is not allowed in sandbox mode".into()),
            }
        );
        // Verify the custom message is present (would be used as tool result).
        match result {
            ToolPermissionResult::Deny { message, .. } => {
                assert_eq!(
                    message.as_deref(),
                    Some("This tool is not allowed in sandbox mode")
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn tool_gate_deny_without_custom_message_uses_none() {
        let decisions = vec![ToolPermission::Deny {
            reason: "forbidden".into(),
            message: None,
        }];
        let result = aggregate_tool_permissions(&decisions);
        match result {
            ToolPermissionResult::Deny { reason, message } => {
                assert_eq!(reason, "forbidden");
                assert!(message.is_none());
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn tool_gate_block_terminates_run() {
        let decisions = vec![ToolPermission::Block {
            reason: "critical policy violation".into(),
        }];
        let result = aggregate_tool_permissions(&decisions);
        assert_eq!(
            result,
            ToolPermissionResult::Block {
                reason: "critical policy violation".into(),
            }
        );
        assert!(result.is_block());
    }

    #[test]
    fn tool_gate_block_overrides_allow() {
        let decisions = vec![
            ToolPermission::Allow,
            ToolPermission::Block {
                reason: "security policy".into(),
            },
            ToolPermission::Allow,
        ];
        let result = aggregate_tool_permissions(&decisions);
        assert_eq!(
            result,
            ToolPermissionResult::Block {
                reason: "security policy".into(),
            }
        );
    }

    #[test]
    fn tool_gate_block_overrides_deny() {
        let decisions = vec![
            ToolPermission::Deny {
                reason: "not allowed".into(),
                message: None,
            },
            ToolPermission::Block {
                reason: "hard block".into(),
            },
        ];
        let result = aggregate_tool_permissions(&decisions);
        assert!(result.is_block());
        assert_eq!(
            result,
            ToolPermissionResult::Block {
                reason: "hard block".into(),
            }
        );
    }

    #[test]
    fn aggregate_all_abstain_yields_suspend() {
        let decisions = vec![ToolPermission::Abstain, ToolPermission::Abstain];
        assert_eq!(
            aggregate_tool_permissions(&decisions),
            ToolPermissionResult::Suspend
        );
    }

    #[test]
    fn aggregate_allow_with_abstain_yields_allow() {
        let decisions = vec![ToolPermission::Allow, ToolPermission::Abstain];
        assert_eq!(
            aggregate_tool_permissions(&decisions),
            ToolPermissionResult::Allow
        );
    }

    #[test]
    fn aggregate_deny_overrides_allow() {
        let decisions = vec![
            ToolPermission::Allow,
            ToolPermission::Deny {
                reason: "nope".into(),
                message: Some("custom msg".into()),
            },
        ];
        let result = aggregate_tool_permissions(&decisions);
        assert!(result.is_deny());
        match result {
            ToolPermissionResult::Deny { reason, message } => {
                assert_eq!(reason, "nope");
                assert_eq!(message.as_deref(), Some("custom msg"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
