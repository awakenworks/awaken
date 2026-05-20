//! Minimal `Plugin` trait impl — pins the descriptor + `register` signature
//! `how-to/add-a-plugin.md` cites. The `PluginRegistrar` is owned by the
//! runtime; user code defines the trait and the runtime drives `register()`
//! during resolve.

use awaken::state::MutationBatch;
use awaken::{AgentSpec, Plugin, PluginDescriptor, PluginRegistrar, StateError};

struct NoopPlugin;

impl Plugin for NoopPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "noop" }
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        Ok(())
    }

    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
    }
}

fn main() {
    let plugin = NoopPlugin;
    let desc = plugin.descriptor();
    assert_eq!(desc.name, "noop");

    // Trait-object hop — the runtime stores plugins as `Arc<dyn Plugin>`.
    let _erased: Box<dyn Plugin> = Box::new(NoopPlugin);
}
