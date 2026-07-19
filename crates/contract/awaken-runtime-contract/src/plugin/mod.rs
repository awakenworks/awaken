//! Plugin mechanism: manifests, capability bounds, resolved contributions, and
//! the per-run execution environment.
//!
//! A `Plugin` is a factory that declares a `PluginManifest` (id, dependencies,
//! config sections, and a `CapabilityBound`) and resolves once into
//! `Contributions`. The runtime merges every active plugin's contributions into a
//! `ResolvedExecutionEnv`, enforcing that each plugin's actual contributions are a
//! subset of its declared bound (G30, fail-closed) and that ids are unique and
//! dependency-ordered. Hooks emit state through the commit path; they never write
//! a store or bypass permission (G9).
//!
//! The mechanism is split by concern, and every public item is re-exported here so
//! `plugin::X` paths stay stable: [`phase`] (hook points and the `PhaseHook`
//! trait), [`capability`] (bounds and their fail-closed enforcement), [`guard`]
//! (run-end continuation), [`contributions`] (the `Plugin` factory and its
//! registrar), and [`env`] (the merged per-run execution environment).

mod capability;
mod contributions;
mod env;
mod guard;
mod phase;

pub use capability::{BoundViolation, CapabilityBound, IdBound, PluginManifest, enforce_bound};
pub use contributions::{Contributions, DynamicTool, Plugin, PluginConfigError};
pub use env::{MergeError, ResolvedExecutionEnv};
pub use guard::{RunEndContext, RunEndDecision, RunEndGuard};
pub use phase::{
    AfterToolContext, ContextMessages, HookReaction, PhaseContext, PhaseHook, PhaseHookPoint,
    PhaseKind,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::Message;
    use awaken_agent_contract::agent::state::Store;

    use super::*;

    struct FakeHook(PhaseHookPoint);

    #[async_trait]
    impl PhaseHook for FakeHook {
        fn point(&self) -> PhaseHookPoint {
            self.0
        }
        async fn on_phase(
            &self,
            _ctx: &PhaseContext,
            _conversation: &[Message],
            _state: &Store,
        ) -> HookReaction {
            HookReaction::default()
        }
    }

    #[test]
    fn id_bound_admits_by_variant() {
        // Any admits everything.
        assert!(IdBound::Any.allows("anything"));
        // Exact admits only listed ids.
        let exact = IdBound::Exact(vec!["a".into(), "b".into()]);
        assert!(exact.allows("a"));
        assert!(!exact.allows("z"));
        // Namespace admits any id under the prefix.
        let ns = IdBound::Namespace("mcp__srv__".into());
        assert!(ns.allows("mcp__srv__echo"));
        assert!(!ns.allows("other__echo"));
        // NamespacedExact requires BOTH the prefix AND the explicit id — a stray id
        // under the prefix that was not discovered fails closed.
        let nse = IdBound::NamespacedExact {
            prefix: "mcp__srv__".into(),
            ids: vec!["mcp__srv__echo".into()],
        };
        assert!(nse.allows("mcp__srv__echo"));
        assert!(!nse.allows("mcp__srv__backdoor")); // prefix ok, not discovered
        assert!(!nse.allows("other__echo")); // discovered-shaped but wrong prefix
    }

    #[test]
    fn id_bound_default_is_deny_all() {
        // An unset axis admits nothing (fail-closed) — the former empty-Vec semantics.
        assert_eq!(IdBound::default(), IdBound::Exact(Vec::new()));
        assert!(!IdBound::default().allows("anything"));
    }

    #[test]
    fn id_bound_is_deny_all_only_for_the_empty_exact_ceiling() {
        // The operator-overlay predicate: only the empty allow-list reads as deny-all;
        // any variant that could admit an id (even an exact list with entries) does not.
        assert!(IdBound::default().is_deny_all());
        assert!(IdBound::Exact(Vec::new()).is_deny_all());
        assert!(!IdBound::Exact(vec!["a".into()]).is_deny_all());
        assert!(!IdBound::Any.is_deny_all());
        assert!(!IdBound::Namespace("mcp__".into()).is_deny_all());
        assert!(
            !IdBound::NamespacedExact {
                prefix: "mcp__".into(),
                ids: Vec::new(),
            }
            .is_deny_all()
        );
    }

    fn manifest(id: &str, bound: CapabilityBound) -> PluginManifest {
        PluginManifest {
            id: id.to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound,
        }
    }

    #[test]
    fn enforce_bound_accepts_a_subset() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Exact(vec!["t".into()]),
                state_keys: IdBound::Exact(vec!["k".into()]),
                phase_hooks: vec![PhaseHookPoint::StepStart],
                action_kinds: IdBound::Exact(vec!["a".into()]),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.tools.push("t".into());
        c.state_keys.push("k".into());
        c.action_kinds.push("a".into());
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepStart)));
        assert!(enforce_bound(&m, &c).is_ok());
    }

    #[test]
    fn enforce_bound_rejects_out_of_bound_contributions() {
        let m = manifest("p", CapabilityBound::default());

        let mut tool = Contributions::new("p");
        tool.tools.push("t".into());
        assert!(matches!(
            enforce_bound(&m, &tool),
            Err(BoundViolation::Tool { .. })
        ));

        let mut key = Contributions::new("p");
        key.state_keys.push("k".into());
        assert!(matches!(
            enforce_bound(&m, &key),
            Err(BoundViolation::StateKey { .. })
        ));

        let mut hook = Contributions::new("p");
        hook.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepEnd)));
        assert!(matches!(
            enforce_bound(&m, &hook),
            Err(BoundViolation::Hook { .. })
        ));

        let mut kind = Contributions::new("p");
        kind.action_kinds.push("a".into());
        assert!(matches!(
            enforce_bound(&m, &kind),
            Err(BoundViolation::ActionKind { .. })
        ));
    }

    struct FakeGate(&'static str);

    #[async_trait]
    impl crate::permission::ToolGateHook for FakeGate {
        fn id(&self) -> &str {
            self.0
        }
        async fn gate(
            &self,
            _ctx: &crate::permission::ToolCall,
            _state: &Store,
        ) -> crate::permission::GateOutcome {
            crate::permission::GateOutcome::Allow
        }
    }

    struct FakeGuard(&'static str);

    #[async_trait]
    impl RunEndGuard for FakeGuard {
        fn id(&self) -> &str {
            self.0
        }
        async fn evaluate(&self, _ctx: &RunEndContext<'_>) -> RunEndDecision {
            RunEndDecision::Complete {
                detail: serde_json::Value::Null,
            }
        }
    }

    #[test]
    fn enforce_bound_rejects_high_privilege_gate_and_guard_ids_outside_the_bound() {
        // The tool-gate and run-end-guard axes are high-privilege (a gate can restrict
        // every tool call); with the default deny-all bound, contributing either id is a
        // fail-closed violation — the two rows the existing enforce_bound table omits.
        let m = manifest("p", CapabilityBound::default());

        let mut gate = Contributions::new("p");
        gate.tool_gates.push(Arc::new(FakeGate("my-gate")));
        assert!(matches!(
            enforce_bound(&m, &gate),
            Err(BoundViolation::ToolGate { id, .. }) if id == "my-gate"
        ));

        let mut guard = Contributions::new("p");
        guard.run_end_guards.push(Arc::new(FakeGuard("my-guard")));
        assert!(matches!(
            enforce_bound(&m, &guard),
            Err(BoundViolation::RunEndGuard { id, .. }) if id == "my-guard"
        ));

        // Declaring the id in the bound admits it.
        let allowed = manifest(
            "p",
            CapabilityBound {
                tool_gates: IdBound::Exact(vec!["my-gate".into()]),
                run_end_guards: IdBound::Exact(vec!["my-guard".into()]),
                ..Default::default()
            },
        );
        let mut both = Contributions::new("p");
        both.tool_gates.push(Arc::new(FakeGate("my-gate")));
        both.run_end_guards.push(Arc::new(FakeGuard("my-guard")));
        assert!(enforce_bound(&allowed, &both).is_ok());
    }

    fn with_action_kind(id: &str, kind: &str) -> (PluginManifest, Contributions) {
        let m = manifest(
            id,
            CapabilityBound {
                action_kinds: IdBound::Exact(vec![kind.into()]),
                ..Default::default()
            },
        );
        let mut c = Contributions::new(id);
        c.action_kinds.push(kind.into());
        (m, c)
    }

    #[test]
    fn merge_collects_action_kinds_and_rejects_duplicates() {
        // A selected plugin's action kind is in the resolved env; an unselected
        // one's is absent (RS-SCH-005).
        let env =
            ResolvedExecutionEnv::merge(vec![with_action_kind("p", "remind")]).expect("merges");
        assert!(env.permits_action_kind("remind"));
        assert!(!env.permits_action_kind("not-contributed"));

        let dup = vec![with_action_kind("a", "k"), with_action_kind("b", "k")];
        assert!(matches!(
            ResolvedExecutionEnv::merge(dup),
            Err(MergeError::DuplicateActionKind { .. })
        ));
    }

    fn with_tool(id: &str, tool: &str) -> (PluginManifest, Contributions) {
        let m = manifest(
            id,
            CapabilityBound {
                tools: IdBound::Exact(vec![tool.into()]),
                ..Default::default()
            },
        );
        let mut c = Contributions::new(id);
        c.tools.push(tool.into());
        (m, c)
    }

    #[test]
    fn merge_rejects_duplicate_tool_ids() {
        let plugins = vec![with_tool("a", "dup"), with_tool("b", "dup")];
        assert!(matches!(
            ResolvedExecutionEnv::merge(plugins),
            Err(MergeError::DuplicateTool { .. })
        ));
    }

    #[test]
    fn merge_rejects_missing_dependency() {
        let mut m = manifest("a", CapabilityBound::default());
        m.requires.push("missing".into());
        let plugins = vec![(m, Contributions::new("a"))];
        assert!(matches!(
            ResolvedExecutionEnv::merge(plugins),
            Err(MergeError::MissingDependency { .. })
        ));
    }

    #[test]
    fn merge_rejects_a_dependency_cycle() {
        let mut a = manifest("a", CapabilityBound::default());
        a.requires.push("b".into());
        let mut b = manifest("b", CapabilityBound::default());
        b.requires.push("a".into());
        let plugins = vec![(a, Contributions::new("a")), (b, Contributions::new("b"))];
        assert_eq!(
            ResolvedExecutionEnv::merge(plugins).err(),
            Some(MergeError::DependencyCycle)
        );
    }

    #[test]
    fn merge_orders_dependencies_first() {
        let mut b = manifest("b", CapabilityBound::default());
        b.requires.push("a".into());
        let a = manifest("a", CapabilityBound::default());
        // Input order is [b, a]; a must come first because b requires it.
        let plugins = vec![(b, Contributions::new("b")), (a, Contributions::new("a"))];
        let env = ResolvedExecutionEnv::merge(plugins).expect("merges");
        assert_eq!(env.order, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn hooks_for_filters_by_point() {
        let m = manifest(
            "p",
            CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::StepStart, PhaseHookPoint::StepEnd],
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepStart)));
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepEnd)));
        let env = ResolvedExecutionEnv::merge(vec![(m, c)]).expect("merges");
        assert_eq!(env.hooks_for(PhaseHookPoint::StepStart).len(), 1);
        assert_eq!(env.hooks_for(PhaseHookPoint::StepEnd).len(), 1);
        assert_eq!(env.hooks_for(PhaseHookPoint::BeforeInference).len(), 0);
    }

    struct FakeRawTool(&'static str);

    #[async_trait]
    impl crate::tool::RawTool for FakeRawTool {
        fn id(&self) -> &str {
            self.0
        }
        async fn invoke(
            &self,
            call: crate::tool::ToolCall,
        ) -> Result<crate::tool::ToolOutput, crate::tool::ToolError> {
            Ok(crate::tool::ToolOutput::ok(call.call_id, "ok"))
        }
    }

    fn dynamic_tool(id: &'static str) -> DynamicTool {
        DynamicTool {
            descriptor: crate::resolved::ToolDescriptor::pinned(
                "mcp",
                id,
                "a dynamic tool",
                serde_json::json!({ "type": "object" }),
            ),
            tool: Arc::new(FakeRawTool(id)),
        }
    }

    #[test]
    fn enforce_bound_accepts_a_dynamic_tool_within_its_namespace() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".into()),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.dynamic_tools.push(dynamic_tool("mcp__srv__echo"));
        assert!(enforce_bound(&m, &c).is_ok());
    }

    #[test]
    fn enforce_bound_rejects_a_dynamic_tool_outside_its_namespace() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".into()),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.dynamic_tools.push(dynamic_tool("other__x"));
        assert!(matches!(
            enforce_bound(&m, &c),
            Err(BoundViolation::Tool { .. })
        ));
    }

    #[test]
    fn merge_exposes_dynamic_descriptors_and_lookup() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".into()),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.dynamic_tools.push(dynamic_tool("mcp__srv__echo"));
        let env = ResolvedExecutionEnv::merge(vec![(m, c)]).expect("merges");
        assert_eq!(env.dynamic_descriptors().len(), 1);
        assert_eq!(env.dynamic_descriptors()[0].id, "mcp__srv__echo");
        assert!(env.dynamic_tool("mcp__srv__echo").is_some());
        assert!(env.dynamic_tool("missing").is_none());
    }
}
