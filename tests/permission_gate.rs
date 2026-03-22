#![allow(missing_docs)]

use awaken::*;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Test helpers: counter state key
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Counter(i64);

struct CounterKey;

impl StateKey for CounterKey {
    const KEY: &'static str = "test.counter";
    type Value = Counter;
    type Update = i64;

    fn apply(value: &mut Counter, delta: i64) {
        value.0 += delta;
    }
}

// ---------------------------------------------------------------------------
// ToolPermissionChecker implementations
// ---------------------------------------------------------------------------

struct AllowAllChecker;

#[async_trait]
impl ToolPermissionChecker for AllowAllChecker {
    async fn check(&self, _ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        Ok(ToolPermission::Allow)
    }
}

struct DenyToolChecker {
    blocked_tools: Vec<String>,
}

#[async_trait]
impl ToolPermissionChecker for DenyToolChecker {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        if let Some(tool_name) = &ctx.tool_name {
            if self.blocked_tools.contains(tool_name) {
                return Ok(ToolPermission::Deny {
                    reason: format!("tool '{}' is blocked", tool_name),
                    message: None,
                });
            }
        }
        Ok(ToolPermission::Abstain)
    }
}

struct ApprovalRequiredChecker {
    require_approval: Vec<String>,
}

#[async_trait]
impl ToolPermissionChecker for ApprovalRequiredChecker {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        if let Some(tool_name) = &ctx.tool_name {
            if self.require_approval.contains(tool_name) {
                return Ok(ToolPermission::Abstain); // No Allow → will Suspend
            }
        }
        Ok(ToolPermission::Allow)
    }
}

struct AbstainChecker;

#[async_trait]
impl ToolPermissionChecker for AbstainChecker {
    async fn check(&self, _ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        Ok(ToolPermission::Abstain)
    }
}

/// Checker that reads state to decide permission.
struct ThresholdChecker {
    max: i64,
}

#[async_trait]
impl ToolPermissionChecker for ThresholdChecker {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        let counter = ctx.state::<CounterKey>().cloned().unwrap_or_default();
        if counter.0 >= self.max {
            Ok(ToolPermission::Deny {
                reason: format!("counter {} exceeds threshold {}", counter.0, self.max),
                message: None,
            })
        } else {
            Ok(ToolPermission::Allow)
        }
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

struct AllowAllPlugin;

impl Plugin for AllowAllPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "allow-all-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_permission("allow-all-plugin", AllowAllChecker)
    }
}

struct DenyPlugin {
    blocked: Vec<String>,
}

impl Plugin for DenyPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "deny-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_permission(
            "deny-plugin",
            DenyToolChecker {
                blocked_tools: self.blocked.clone(),
            },
        )
    }
}

struct ApprovalPlugin {
    require_approval: Vec<String>,
}

impl Plugin for ApprovalPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "approval-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_permission(
            "approval-plugin",
            ApprovalRequiredChecker {
                require_approval: self.require_approval.clone(),
            },
        )
    }
}

struct AbstainPlugin;

impl Plugin for AbstainPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "abstain-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_permission("abstain-plugin", AbstainChecker)
    }
}

struct ThresholdPlugin {
    max: i64,
}

impl Plugin for ThresholdPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "threshold-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<CounterKey>(StateKeyOptions::default())?;
        registrar.register_tool_permission("threshold-plugin", ThresholdChecker { max: self.max })
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn tool_ctx(runtime: &PhaseRuntime, tool_name: &str) -> PhaseContext {
    PhaseContext::new(Phase::BeforeToolExecute, runtime.store().snapshot())
        .with_tool_info(tool_name, "call-1", None)
}

// ---------------------------------------------------------------------------
// Tests: aggregate_tool_permissions
// ---------------------------------------------------------------------------

#[test]
fn aggregate_empty_decisions_suspends() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[]);
    assert!(result.is_suspend());
}

#[test]
fn aggregate_all_abstain_suspends() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[ToolPermission::Abstain, ToolPermission::Abstain]);
    assert!(result.is_suspend());
}

#[test]
fn aggregate_any_allow_allows() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[
        ToolPermission::Abstain,
        ToolPermission::Allow,
        ToolPermission::Abstain,
    ]);
    assert!(result.is_allow());
}

#[test]
fn aggregate_deny_wins_over_allow() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[
        ToolPermission::Allow,
        ToolPermission::Deny {
            reason: "blocked".into(),
            message: None,
        },
        ToolPermission::Allow,
    ]);
    assert!(result.is_deny());
}

#[test]
fn aggregate_deny_wins_over_abstain() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[
        ToolPermission::Abstain,
        ToolPermission::Deny {
            reason: "no".into(),
            message: None,
        },
    ]);
    assert!(result.is_deny());
}

#[test]
fn aggregate_single_allow() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[ToolPermission::Allow]);
    assert!(result.is_allow());
}

#[test]
fn aggregate_single_deny() {
    use awaken::aggregate_tool_permissions;
    let result = aggregate_tool_permissions(&[ToolPermission::Deny {
        reason: "x".into(),
        message: None,
    }]);
    assert!(result.is_deny());
    assert_eq!(
        result,
        ToolPermissionResult::Deny {
            reason: "x".into(),
            message: None
        }
    );
}

// ---------------------------------------------------------------------------
// Tests: ToolPermission helpers
// ---------------------------------------------------------------------------

#[test]
fn tool_permission_helpers() {
    let allow = ToolPermission::Allow;
    assert!(allow.is_allow());
    assert!(!allow.is_deny());
    assert!(!allow.is_abstain());

    let deny = ToolPermission::Deny {
        reason: "x".into(),
        message: None,
    };
    assert!(!deny.is_allow());
    assert!(deny.is_deny());
    assert!(!deny.is_abstain());

    let abstain = ToolPermission::Abstain;
    assert!(!abstain.is_allow());
    assert!(!abstain.is_deny());
    assert!(abstain.is_abstain());
}

#[test]
fn tool_permission_result_helpers() {
    let allow = ToolPermissionResult::Allow;
    assert!(allow.is_allow());
    assert!(!allow.is_deny());
    assert!(!allow.is_suspend());

    let deny = ToolPermissionResult::Deny {
        reason: "x".into(),
        message: None,
    };
    assert!(!deny.is_allow());
    assert!(deny.is_deny());
    assert!(!deny.is_suspend());

    let suspend = ToolPermissionResult::Suspend;
    assert!(!suspend.is_allow());
    assert!(!suspend.is_deny());
    assert!(suspend.is_suspend());
}

// ---------------------------------------------------------------------------
// Tests: PhaseRuntime.check_tool_permission
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_checkers_suspends() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let env = ExecutionEnv::empty();

    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "anything"))
        .await
        .unwrap();
    assert!(result.is_suspend());
}

#[tokio::test]
async fn allow_all_plugin_allows_everything() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(AllowAllPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "anything"))
        .await
        .unwrap();
    assert!(result.is_allow());
}

#[tokio::test]
async fn deny_plugin_blocks_specific_tool() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(AllowAllPlugin),
        Arc::new(DenyPlugin {
            blocked: vec!["rm".into()],
        }),
    ];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    // rm is blocked
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "rm"))
        .await
        .unwrap();
    assert!(result.is_deny());
    assert_eq!(
        result,
        ToolPermissionResult::Deny {
            reason: "tool 'rm' is blocked".into(),
            message: None,
        }
    );

    // read is allowed (deny plugin abstains, allow plugin allows)
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "read"))
        .await
        .unwrap();
    assert!(result.is_allow());
}

#[tokio::test]
async fn deny_overrides_allow() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(AllowAllPlugin),
        Arc::new(DenyPlugin {
            blocked: vec!["dangerous".into()],
        }),
    ];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "dangerous"))
        .await
        .unwrap();
    assert!(result.is_deny());
}

#[tokio::test]
async fn no_allow_means_suspend() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    // Only abstain checker, no allow
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(AbstainPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "anything"))
        .await
        .unwrap();
    assert!(result.is_suspend());
}

#[tokio::test]
async fn approval_required_suspends_specific_tools() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(ApprovalPlugin {
        require_approval: vec!["delete_file".into()],
    })];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    // delete_file: approval checker abstains (no allow) → suspend
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "delete_file"))
        .await
        .unwrap();
    assert!(result.is_suspend());

    // read: approval checker allows → allow
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "read"))
        .await
        .unwrap();
    assert!(result.is_allow());
}

#[tokio::test]
async fn state_dependent_checker() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    runtime
        .store()
        .install_plugin(ThresholdPlugin { max: 2 })
        .unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(ThresholdPlugin { max: 2 })];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    // Counter = 0 → allow
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "tool"))
        .await
        .unwrap();
    assert!(result.is_allow());

    // Increment counter to 2
    let mut patch = awaken::MutationBatch::new();
    patch.update::<CounterKey>(2);
    runtime.store().commit(patch).unwrap();

    // Counter = 2 → deny
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "tool"))
        .await
        .unwrap();
    assert!(result.is_deny());
}

#[tokio::test]
async fn uninstall_removes_checker() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();

    // With AllowAllPlugin
    let plugins_with: Vec<Arc<dyn Plugin>> = vec![Arc::new(AllowAllPlugin)];
    let env_with = ExecutionEnv::from_plugins(&plugins_with).unwrap();

    let result = runtime
        .check_tool_permission(&env_with, &tool_ctx(&runtime, "x"))
        .await
        .unwrap();
    assert!(result.is_allow());

    // Without AllowAllPlugin (simulates uninstall by rebuilding env)
    let env_without = ExecutionEnv::empty();

    // No checkers → suspend
    let result = runtime
        .check_tool_permission(&env_without, &tool_ctx(&runtime, "x"))
        .await
        .unwrap();
    assert!(result.is_suspend());
}

#[tokio::test]
async fn multiple_checkers_all_allow() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();

    struct AllowAllPlugin2;
    impl Plugin for AllowAllPlugin2 {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "allow-all-2",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_tool_permission("allow-all-2", AllowAllChecker)
        }
    }

    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(AllowAllPlugin), Arc::new(AllowAllPlugin2)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "x"))
        .await
        .unwrap();
    assert!(result.is_allow());
}

#[tokio::test]
async fn mixed_allow_and_abstain_allows() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(AllowAllPlugin), Arc::new(AbstainPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    // AllowAll allows, Abstain abstains → Allow wins
    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "x"))
        .await
        .unwrap();
    assert!(result.is_allow());
}

// ---------------------------------------------------------------------------
// Tests: AllowAllToolsPlugin (built-in)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn builtin_allow_all_tools_plugin() {
    use awaken::agent::tool_permission::AllowAllToolsPlugin;

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(AllowAllToolsPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let result = runtime
        .check_tool_permission(&env, &tool_ctx(&runtime, "anything"))
        .await
        .unwrap();
    assert!(result.is_allow());
}

use std::sync::Arc;
