use std::sync::Arc;

use awaken_contract::StateMap;
use awaken_contract::contract::tool::Tool;
use awaken_contract::model::{Phase, ScheduledActionSpec};
use awaken_contract::state::Snapshot;
use awaken_runtime::agent::state::ExcludeTool;
use awaken_runtime::registry::{
    MapAgentSpecRegistry, MapBackendRegistry, MapModelRegistry, MapPluginSource,
    MapProviderRegistry, RegistrySet, RegistrySnapshot, ToolRegistry,
};
use awaken_runtime::state::StateCommand;
use awaken_runtime::{PhaseContext, PhaseHook};

use crate::rules::{PermissionRule, ToolCallPattern, ToolPermissionBehavior};
use crate::state::{PermissionPolicy, PermissionPolicyKey};

use super::filter::PermissionToolFilterHook;

fn snapshot_with_policy(policy: PermissionPolicy) -> Snapshot {
    let mut state_map = StateMap::default();
    state_map.insert::<PermissionPolicyKey>(policy);
    Snapshot::new(0, Arc::new(state_map))
}

fn make_ctx(snapshot: Snapshot) -> PhaseContext {
    PhaseContext::new(Phase::BeforeInference, snapshot)
}

/// Minimal `ToolRegistry` for tests — `ToolRegistry::tool_ids` is the
/// only entry point the permission filter walks. `get_tool` returns
/// `None` because none of the hooks under test resolve concrete tools.
struct StubToolRegistry(Vec<String>);

impl ToolRegistry for StubToolRegistry {
    fn get_tool(&self, _id: &str) -> Option<Arc<dyn Tool>> {
        None
    }
    fn tool_ids(&self) -> Vec<String> {
        self.0.clone()
    }
}

fn registry_snapshot_with_tools<I, S>(ids: I) -> Arc<RegistrySnapshot>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let ids: Vec<String> = ids.into_iter().map(Into::into).collect();
    let registries = RegistrySet {
        agents: Arc::new(MapAgentSpecRegistry::new()),
        tools: Arc::new(StubToolRegistry(ids)),
        models: Arc::new(MapModelRegistry::new()),
        providers: Arc::new(MapProviderRegistry::new()),
        plugins: Arc::new(MapPluginSource::new()),
        backends: Arc::new(MapBackendRegistry::new()),
    };
    Arc::new(RegistrySnapshot::new(0, registries))
}

/// Read the `ExcludeTool` payloads from a `StateCommand`'s scheduled
/// actions, sorted for stable comparison.
fn scheduled_exclude_tool_ids(cmd: &StateCommand) -> Vec<String> {
    let mut out: Vec<String> = cmd
        .scheduled_actions()
        .iter()
        .filter(|a| a.key == ExcludeTool::KEY)
        .map(|a| ExcludeTool::decode_payload(a.payload.clone()).expect("decode ExcludeTool"))
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn no_rules_yields_no_exclusions() {
    let ctx = make_ctx(snapshot_with_policy(PermissionPolicy::default()));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert!(cmd.scheduled_actions().is_empty());
}

#[tokio::test]
async fn unconditional_deny_excludes_tool() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("dangerous_tool", ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();

    assert_eq!(cmd.scheduled_actions().len(), 1);
    assert_eq!(cmd.scheduled_actions()[0].key, ExcludeTool::KEY);
    let payload: String =
        ExcludeTool::decode_payload(cmd.scheduled_actions()[0].payload.clone()).unwrap();
    assert_eq!(payload, "dangerous_tool");
}

#[tokio::test]
async fn multiple_unconditional_denies_produce_multiple_exclusions() {
    let mut policy = PermissionPolicy::default();
    for name in ["rm", "shutdown", "drop_table"] {
        let rule = PermissionRule::new_tool(name, ToolPermissionBehavior::Deny);
        policy.rules.insert(rule.subject.key(), rule);
    }

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();

    let mut excluded: Vec<String> = cmd
        .scheduled_actions()
        .iter()
        .map(|a| ExcludeTool::decode_payload(a.payload.clone()).unwrap())
        .collect();
    excluded.sort();
    assert_eq!(excluded, vec!["drop_table", "rm", "shutdown"]);
}

#[tokio::test]
async fn allow_rule_does_not_exclude() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("safe_tool", ToolPermissionBehavior::Allow);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert!(cmd.scheduled_actions().is_empty());
}

#[tokio::test]
async fn ask_rule_does_not_exclude() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("ask_tool", ToolPermissionBehavior::Ask);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert!(cmd.scheduled_actions().is_empty());
}

#[tokio::test]
async fn conditional_deny_does_not_exclude() {
    use awaken_tool_pattern::ToolCallPattern;

    let mut policy = PermissionPolicy::default();
    // Pattern with argument condition: `Edit(file_path ~ "/etc/*")`
    let pattern = ToolCallPattern::tool_with_primary("Edit", "/etc/*");
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    // Conditional deny should NOT be excluded at BeforeInference
    assert!(cmd.scheduled_actions().is_empty());
}

#[tokio::test]
async fn empty_state_yields_no_exclusions() {
    // No policy in state at all
    let snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
    let ctx = make_ctx(snapshot);
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert!(cmd.scheduled_actions().is_empty());
}

#[tokio::test]
async fn mixed_rules_only_excludes_unconditional_denies() {
    use awaken_tool_pattern::ToolCallPattern;

    let mut policy = PermissionPolicy::default();

    // Unconditional deny
    let rule = PermissionRule::new_tool("rm", ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    // Allow
    let rule = PermissionRule::new_tool("read", ToolPermissionBehavior::Allow);
    policy.rules.insert(rule.subject.key(), rule);

    // Ask
    let rule = PermissionRule::new_tool("write", ToolPermissionBehavior::Ask);
    policy.rules.insert(rule.subject.key(), rule);

    // Conditional deny
    let pattern = ToolCallPattern::tool_with_primary("Bash", "rm *");
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();

    // Only "rm" should be excluded
    assert_eq!(cmd.scheduled_actions().len(), 1);
    let payload: String =
        ExcludeTool::decode_payload(cmd.scheduled_actions()[0].payload.clone()).unwrap();
    assert_eq!(payload, "rm");
}

// R13 — Glob/regex any-args Deny expansion against the live tool registry.
// The runtime BeforeInference hook MUST schedule `ExcludeTool` for every
// registry id matched by such a rule, in lock-step with the admin-console
// permission preview. Otherwise preview claims "X tools stripped before
// the model sees the list" while the model still sees those tools, and
// the block falls back to ToolGate at call time — a semantic regression.

#[tokio::test]
async fn glob_deny_expands_against_registry() {
    let mut policy = PermissionPolicy::default();
    let pattern = ToolCallPattern::tool_glob("mcp__db__*");
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let snapshot = registry_snapshot_with_tools([
        "Bash",
        "mcp__db__query",
        "mcp__db__write",
        "mcp__github__list_issues",
    ]);
    let ctx = make_ctx(snapshot_with_policy(policy)).with_registry_snapshot(snapshot);
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert_eq!(
        scheduled_exclude_tool_ids(&cmd),
        vec!["mcp__db__query".to_string(), "mcp__db__write".to_string()],
    );
}

#[tokio::test]
async fn regex_deny_expands_against_registry() {
    let mut policy = PermissionPolicy::default();
    let pattern = crate::rules::parse_pattern("/mcp__(gh|gl)__.*/").expect("valid regex pattern");
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let snapshot = registry_snapshot_with_tools([
        "Bash",
        "mcp__gh__list_repos",
        "mcp__gl__list_projects",
        "mcp__db__query",
    ]);
    let ctx = make_ctx(snapshot_with_policy(policy)).with_registry_snapshot(snapshot);
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert_eq!(
        scheduled_exclude_tool_ids(&cmd),
        vec![
            "mcp__gh__list_repos".to_string(),
            "mcp__gl__list_projects".to_string(),
        ],
    );
}

#[tokio::test]
async fn glob_deny_without_registry_falls_back_to_exact_only() {
    // Minimal test contexts (and any phase that runs before the runner
    // attaches the registry snapshot) carry no registry. The hook must
    // degrade gracefully to exact-tool Deny stripping rather than panic
    // or treat the missing registry as "deny all".
    let mut policy = PermissionPolicy::default();
    // Glob Deny — has no effect without a registry to expand against.
    let glob = PermissionRule::new_pattern(
        ToolCallPattern::tool_glob("mcp__db__*"),
        ToolPermissionBehavior::Deny,
    );
    policy.rules.insert(glob.subject.key(), glob);
    // Exact-tool Deny — still applies.
    let exact = PermissionRule::new_tool("rm", ToolPermissionBehavior::Deny);
    policy.rules.insert(exact.subject.key(), exact);

    let ctx = make_ctx(snapshot_with_policy(policy));
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert_eq!(scheduled_exclude_tool_ids(&cmd), vec!["rm".to_string()]);
}

#[tokio::test]
async fn glob_deny_does_not_expand_per_args_rules() {
    // `Bash(npm *)` Deny is conditional on args — the runtime filter
    // must NOT strip `Bash` from the tool list. ToolGate handles per-args
    // rules at call time.
    let mut policy = PermissionPolicy::default();
    let pattern = ToolCallPattern::tool_with_primary("Bash", "rm *");
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let snapshot = registry_snapshot_with_tools(["Bash", "Read"]);
    let ctx = make_ctx(snapshot_with_policy(policy)).with_registry_snapshot(snapshot);
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    assert!(scheduled_exclude_tool_ids(&cmd).is_empty());
}

/// **Cross-layer parity invariant** — the runtime BeforeInference hook's
/// scheduled `ExcludeTool` set MUST equal
/// `PermissionRuleset::unconditionally_denied_against(registry)`. Both
/// the runtime filter and the admin-console permission preview share the
/// same helper, so this test pins the contract: if the hook is ever
/// refactored to compute its own set (or stop honoring the registry), the
/// test fires and signals the drift before preview and runtime disagree
/// about what "stripped before the model sees the list" means.
#[tokio::test]
async fn hook_and_helper_agree_on_unconditional_deny_set() {
    let mut policy = PermissionPolicy::default();
    // A mix of every flavor the helper knows about.
    policy.rules.insert(
        "tool:rm".into(),
        PermissionRule::new_tool("rm", ToolPermissionBehavior::Deny),
    );
    let glob = PermissionRule::new_pattern(
        ToolCallPattern::tool_glob("mcp__db__*"),
        ToolPermissionBehavior::Deny,
    );
    policy.rules.insert(glob.subject.key(), glob);
    let regex_rule = PermissionRule::new_pattern(
        crate::rules::parse_pattern("/mcp__(gh|gl)__.*/").expect("valid regex"),
        ToolPermissionBehavior::Deny,
    );
    policy.rules.insert(regex_rule.subject.key(), regex_rule);
    // Conditional / non-deny rules — must not contribute.
    let cond = PermissionRule::new_pattern(
        ToolCallPattern::tool_with_primary("Bash", "rm *"),
        ToolPermissionBehavior::Deny,
    );
    policy.rules.insert(cond.subject.key(), cond);
    let allow = PermissionRule::new_pattern(
        ToolCallPattern::tool_glob("Read*"),
        ToolPermissionBehavior::Allow,
    );
    policy.rules.insert(allow.subject.key(), allow);

    let tool_ids = vec![
        "rm".to_string(),
        "Bash".to_string(),
        "Read".to_string(),
        "ReadFile".to_string(),
        "mcp__db__query".to_string(),
        "mcp__db__write".to_string(),
        "mcp__gh__list_repos".to_string(),
        "mcp__gl__list_projects".to_string(),
        "mcp__github__list_issues".to_string(),
    ];
    let snapshot = registry_snapshot_with_tools(tool_ids.clone());

    // Helper output (the shared source of truth that preview uses).
    let ruleset = crate::state::permission_rules_from_state(Some(&policy), None);
    let mut helper_set: Vec<String> = ruleset
        .unconditionally_denied_against(&tool_ids)
        .into_iter()
        .collect();
    helper_set.sort();

    // Runtime hook output.
    let ctx = make_ctx(snapshot_with_policy(policy)).with_registry_snapshot(snapshot);
    let cmd = PermissionToolFilterHook.run(&ctx).await.unwrap();
    let hook_set = scheduled_exclude_tool_ids(&cmd);

    assert_eq!(
        hook_set, helper_set,
        "BeforeInference hook diverged from PermissionRuleset::unconditionally_denied_against — \
         admin-console preview and runtime would disagree about stripped tools"
    );
    // Spot-check: every flavor that should strip is in the set.
    assert!(hook_set.contains(&"rm".to_string()));
    assert!(hook_set.contains(&"mcp__db__query".to_string()));
    assert!(hook_set.contains(&"mcp__gh__list_repos".to_string()));
    // Spot-check: nothing else slipped in.
    assert!(!hook_set.contains(&"Bash".to_string()));
    assert!(!hook_set.contains(&"Read".to_string()));
    assert!(!hook_set.contains(&"mcp__github__list_issues".to_string()));
}
