//! Original MarkHook fixture, shared by input-forwarding regressions.
//! Recording is observational: it owns no Runtime or committed state.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope, Store};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, HookReaction, IdBound, PhaseContext, PhaseHook, PhaseHookPoint,
    Plugin, PluginManifest,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

type RecordedRunInputs = Arc<Mutex<Vec<Vec<Message>>>>;

/// A hook that stages the current Run input at BeforeInference.
pub(super) struct MarkHook {
    pub(super) inputs: Option<RecordedRunInputs>,
}

#[async_trait::async_trait]
impl PhaseHook for MarkHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }
    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        _conversation: &[Message],
        _state: &Store,
    ) -> HookReaction {
        let awaken_runtime_contract::plugin::PhaseKind::BeforeInference { run_input } = &ctx.kind
        else {
            unreachable!("MarkHook is registered only at BeforeInference")
        };
        if let Some(inputs) = &self.inputs {
            inputs.lock().unwrap().push(run_input.to_vec());
            return HookReaction::default();
        }
        HookReaction::state(vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "phase",
            serde_json::json!({
                "phase": format!("{:?}@{}", ctx.kind.point(), ctx.step),
                "run_input": run_input.iter().map(Message::text_content).collect::<Vec<_>>(),
            }),
        )])
    }
}

/// A well-behaved plugin: declares the hook point and state key it uses, and
/// counts how many times it is resolved.
pub(super) struct MarkPlugin {
    pub(super) resolves: Arc<AtomicUsize>,
    pub(super) inputs: Option<RecordedRunInputs>,
}

impl MarkPlugin {
    pub(super) fn recording() -> (Arc<Self>, RecordedRunInputs) {
        let inputs = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                resolves: Arc::new(AtomicUsize::new(0)),
                inputs: Some(inputs.clone()),
            }),
            inputs,
        )
    }
}

impl Plugin for MarkPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "mark".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                state_keys: IdBound::Exact(vec!["phase".to_string()]),
                phase_hooks: vec![PhaseHookPoint::BeforeInference],
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let mut c = Contributions::new("mark");
        c.state_keys.push("phase".to_string());
        c.phase_hooks.push(Arc::new(MarkHook {
            inputs: self.inputs.clone(),
        }));
        c
    }
}
