//! Composition-root wiring for skills.
//!
//! Skills bridge three things only the host sees together: the sandbox
//! environment (to discover workspace skills and resolve `${SKILL_DIR}`), the
//! sub-run capability (for `context: fork`), and the base permission gate (to
//! observe touched paths). Everything skill-*behavioral* lives in
//! `awaken-ext-skills`; this module only wires those host-owned pieces to the
//! extension's ports and assembles the two tools for a thread.

use std::path::PathBuf;
use std::sync::Arc;

use awaken_ext_skills::{
    CompositeSkillRegistry, InMemorySkillRegistry, ListSkillsTool, PathActivations, RecordingGate,
    SkillFile, SkillProvenance, SkillRegistry, SkillSource, SkillSpec, SkillTool,
    SourceSkillRegistry, SubAgentRunner,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::permission::ToolGateHook;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{Environment, LocalSandboxProvider};

/// The conventional workspace subdir the agent authors skills under; scanned live
/// so a skill written this run is discovered (ADR-0036 D8).
const WORKSPACE_SKILLS_SUBDIR: &str = "skills";

/// Bridges the sandbox [`Environment`] to the [`SkillSource`] port: scans the
/// workspace skill dir live, returning neutral file data. The host owns this bridge
/// so `awaken-ext-skills` stays sandbox-unaware and the root stays hidden.
struct EnvSkillSource {
    env: Arc<Environment>,
    subdir: String,
}

impl SkillSource for EnvSkillSource {
    fn scan(&self) -> Vec<SkillFile> {
        self.env
            .scan_skill_dir(&self.subdir)
            .into_iter()
            .map(|f| SkillFile {
                id: f.id,
                content: f.content,
                dir: Some(f.dir),
            })
            .collect()
    }
}

/// Bridges the durable [`awaken_skill_store::SkillStore`] to the [`SkillSource`] port:
/// scans the persisted catalog live, returning each `SKILL.md` as neutral file data.
/// The host owns this bridge so `awaken-ext-skills` stays store-unaware — it sees only
/// `SkillFile`s, never the store. Scanned on every query so a skill added through the
/// `/v1/skills` API this run is discovered without a rebuild.
struct StoreSkillSource {
    store: Arc<awaken_skill_store::SkillStore>,
}

impl SkillSource for StoreSkillSource {
    fn scan(&self) -> Vec<SkillFile> {
        self.store
            .list()
            .into_iter()
            .map(|(id, content)| SkillFile {
                id,
                content,
                dir: None,
            })
            .collect()
    }
}

/// Runs a `context: fork` skill as a fresh, isolated sub-agent, returning its
/// reply. Bridges the [`SubAgentRunner`] port to the shared `run_subagent`.
struct ForkRunner {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalSandboxProvider,
}

#[async_trait::async_trait]
impl SubAgentRunner for ForkRunner {
    async fn run(&self, skill_id: &str, prompt: &str) -> Result<String, String> {
        let name = format!("skill-{skill_id}");
        // A `context: fork` skill runs its own isolated sub-thread; its usage stays
        // there (this port surfaces only the reply text).
        crate::subagent::run_subagent(
            self.llm.clone(),
            &self.model_ref,
            &self.provider,
            &name,
            prompt,
            None,
        )
        .await
        .map(|(text, _usage)| text)
    }
}

/// The wired skill surface for one thread: the two tools, their descriptors, the
/// registry (for `/name` expansion), and the base gate wrapped to observe paths.
pub(crate) struct SkillWiring {
    pub registry: Arc<dyn SkillRegistry>,
    pub descriptors: Vec<ToolDescriptor>,
    pub list_tool: Arc<dyn RawTool>,
    pub activate_tool: Arc<dyn RawTool>,
    pub gate: Arc<dyn ToolGateHook>,
}

/// Assemble the skill surface for a thread, or `None` when no skills are offered.
/// `base_gate` is wrapped so conditional (`paths`) skills surface on file touch;
/// `fork_base` is the sub-agent sandbox base for `context: fork` skills.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wire_skills(
    configured: &[SkillSpec],
    skill_store: Option<Arc<awaken_skill_store::SkillStore>>,
    env: Arc<Environment>,
    llm: Arc<dyn LlmExecutor>,
    model_ref: &str,
    session_id: &str,
    base_gate: Arc<dyn ToolGateHook>,
    fork_base: PathBuf,
) -> Option<SkillWiring> {
    // Offer skills when either a static set is configured or a durable catalog is
    // wired — the workspace-authored source alone never opens the surface (a run with
    // no delivered skills shows nothing until the agent authors one it can re-read).
    if configured.is_empty() && skill_store.is_none() {
        return None;
    }
    // Delivered skills come from two trusted sources: the static configured set and —
    // when wired — the durable `/v1/skills` catalog (both `Delivered` provenance),
    // plus a live scan of the workspace for skills the agent authored this run
    // (`AgentCreated`). Static wins over durable wins over authored on a duplicate id.
    let mut registries: Vec<Arc<dyn SkillRegistry>> = Vec::new();
    if !configured.is_empty() {
        registries.push(Arc::new(InMemorySkillRegistry::from_specs(
            configured.iter().cloned(),
        )));
    }
    if let Some(store) = skill_store {
        registries.push(Arc::new(SourceSkillRegistry::new(
            Arc::new(StoreSkillSource { store }),
            SkillProvenance::Delivered,
        )));
    }
    registries.push(Arc::new(SourceSkillRegistry::new(
        Arc::new(EnvSkillSource {
            env,
            subdir: WORKSPACE_SKILLS_SUBDIR.to_string(),
        }),
        SkillProvenance::AgentCreated,
    )));
    let registry: Arc<dyn SkillRegistry> = Arc::new(CompositeSkillRegistry::new(registries));

    let activations = PathActivations::new();
    let gate: Arc<dyn ToolGateHook> = Arc::new(RecordingGate::new(base_gate, activations.clone()));
    let list: Arc<dyn RawTool> =
        Arc::new(ListSkillsTool::new(registry.clone()).with_path_activations(activations));
    let fork_runner: Arc<dyn SubAgentRunner> = Arc::new(ForkRunner {
        llm,
        model_ref: model_ref.to_string(),
        provider: LocalSandboxProvider::new(fork_base),
    });
    let activate: Arc<dyn RawTool> = Arc::new(
        SkillTool::new(registry.clone())
            .with_session_id(session_id)
            .with_fork_runner(fork_runner),
    );

    Some(SkillWiring {
        descriptors: vec![list_skills_descriptor(), skill_descriptor()],
        list_tool: list,
        activate_tool: activate,
        gate,
        registry,
    })
}

fn list_skills_descriptor() -> ToolDescriptor {
    awaken_ext_skills::list_skills_tool_descriptor()
}

fn skill_descriptor() -> ToolDescriptor {
    awaken_ext_skills::skill_tool_descriptor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_sandbox_local::{SandboxProvider, SandboxSpec};

    #[test]
    fn durable_store_source_scans_the_catalog_as_delivered_skill_files() {
        // The host's bridge over the durable skill store yields neutral SkillFiles the
        // extension parses — so a skill persisted in the store is offered as Delivered
        // without awaken-ext-skills ever seeing the store.
        let root = std::env::temp_dir().join(format!("awaken-skillsrc-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let store = Arc::new(awaken_skill_store::SkillStore::open(&root).unwrap());
        store
            .put("greet", "---\ndescription: say hi\n---\nHELLO")
            .unwrap();

        let source = StoreSkillSource {
            store: store.clone(),
        };
        let files = source.scan();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "greet");
        assert!(files[0].content.contains("HELLO"));

        let reg = SourceSkillRegistry::new(Arc::new(source), SkillProvenance::Delivered);
        let spec = reg.get("greet").expect("delivered skill resolves");
        assert_eq!(spec.description, "say hi");
        assert_eq!(spec.provenance, SkillProvenance::Delivered);
        assert!(spec.body.contains("HELLO"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn agent_authored_skill_is_discovered_live_from_the_workspace() {
        // A skill the agent writes under the workspace this run is discovered live,
        // tagged AgentCreated (ADR-0036 D6/D8), without rebuilding or exposing root.
        let base = std::env::temp_dir().join(format!("awaken-authored-{}", std::process::id()));
        let provider = LocalSandboxProvider::new(&base);
        let env = Arc::new(provider.create(&SandboxSpec::new("t")).await.unwrap());

        let registry = SourceSkillRegistry::new(
            Arc::new(EnvSkillSource {
                env: env.clone(),
                subdir: WORKSPACE_SKILLS_SUBDIR.to_string(),
            }),
            SkillProvenance::AgentCreated,
        );
        assert!(registry.list().is_empty(), "nothing authored yet");

        let skill_dir = base.join("t").join(WORKSPACE_SKILLS_SUBDIR).join("notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: my notes\n---\nremember to hydrate",
        )
        .unwrap();

        let found = registry
            .get("notes")
            .expect("authored skill discovered live");
        assert_eq!(found.description, "my notes");
        assert_eq!(found.provenance, SkillProvenance::AgentCreated);
        assert_eq!(found.dir.as_deref(), Some("skills/notes"));
        assert!(found.body.contains("hydrate"));

        provider.teardown("t").await.unwrap();
    }
}
