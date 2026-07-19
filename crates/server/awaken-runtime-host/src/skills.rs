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
    SourceSkillRegistry,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::permission::ToolGateHook;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::subagent_runner::{
    SubagentError, SubagentReply, SubagentRequest, SubagentRunner,
};
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

/// The default workspace subdir the agent authors skills under, scanned live so a
/// skill written this run is discovered (ADR-0036 D8). A hand/agent definition can
/// negotiate a different dir via its `plugin_config.skills_dir`; this is the fallback.
pub(crate) const DEFAULT_SKILLS_SUBDIR: &str = "skills";

/// Bridges the sandbox [`LocalSandbox`] to the [`SkillSource`] port: scans the
/// workspace skill dir live, returning neutral file data. The host owns this bridge
/// so `awaken-ext-skills` stays sandbox-unaware and the root stays hidden.
struct EnvSkillSource {
    env: Arc<LocalSandbox>,
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

/// Bridges a snapshot of the durable delivered catalog to the [`SkillSource`] port,
/// returning each `SKILL.md` as neutral file data. The host owns this bridge so
/// `awaken-ext-skills` stays store-unaware — it sees only `SkillFile`s, never the
/// store. The snapshot is loaded (async) from the [`SkillStore`] at session setup —
/// the run-loop scan is synchronous, so a network-DB (async) catalog cannot be hit
/// per query; it is read once into this snapshot instead.
///
/// [`SkillStore`]: awaken_skill_store::SkillStore
struct SnapshotSkillSource {
    files: Vec<SkillFile>,
}

impl SkillSource for SnapshotSkillSource {
    fn scan(&self) -> Vec<SkillFile> {
        self.files.clone()
    }
}

/// Runs a `context: fork` skill as an Agent with the capabilities declared by its
/// config, returning its reply. By default it shares the parent Agent's sandbox
/// (`默认共用`), so a forked skill sees the same workspace; with
/// `reuse_sandbox` off it gets a fresh, isolated root. Implements the neutral
/// [`SubagentRunner`] port — the same one the goal judge and compactor use.
struct ForkRunner {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalProvider,
    /// The parent agent's sandbox, shared with the fork by default.
    sandbox: Arc<LocalSandbox>,
    /// Reuse the parent sandbox (default) vs. a fresh, isolated one.
    reuse_sandbox: bool,
}

#[async_trait::async_trait]
impl SubagentRunner for ForkRunner {
    async fn run(&self, request: SubagentRequest) -> Result<SubagentReply, SubagentError> {
        // The skill id names the sub-run; the seed is the resolved skill body. Skill
        // activation is out-of-band housekeeping, so its usage stays isolated (this
        // port surfaces only the reply text).
        let name = format!("skill-{}", request.agent_id);
        let sandbox = if self.reuse_sandbox {
            crate::subagent::AgentRunSandbox::Shared(&self.sandbox)
        } else {
            crate::subagent::AgentRunSandbox::Fresh(&self.provider)
        };
        let delegates = std::collections::HashSet::new();
        crate::subagent::run_agent(
            self.llm.clone(),
            crate::subagent::AgentExecution {
                agent_id: &request.agent_id,
                model_ref: &self.model_ref,
                delegates: &delegates,
                delegation_executor: None,
                context: None,
            },
            sandbox,
            crate::subagent::AgentRunIdentity::transient(&name),
            request.seed,
            request.cancellation,
            crate::subagent::UsageRollup::Isolated,
        )
        .await
        .map(|(text, _usage)| SubagentReply { text: Some(text) })
        .map_err(SubagentError)
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
    delivered: Option<Vec<(String, String)>>,
    env: Arc<LocalSandbox>,
    llm: Arc<dyn LlmExecutor>,
    model_ref: &str,
    session_id: &str,
    base_gate: Arc<dyn ToolGateHook>,
    fork_base: PathBuf,
    reuse_sandbox: bool,
    skills_subdir: &str,
) -> Option<SkillWiring> {
    // Offer skills when either a static set is configured or a durable catalog is
    // wired — the workspace-authored source alone never opens the surface (a run with
    // no delivered skills shows nothing until the agent authors one it can re-read).
    // `delivered` is `Some` (possibly empty) exactly when a durable store is wired.
    if configured.is_empty() && delivered.is_none() {
        return None;
    }
    // Delivered skills come from two trusted sources: the static configured set and —
    // when wired — the durable `/v1/skills` catalog snapshot (both `Delivered`
    // provenance), plus a live scan of the workspace for skills the agent authored
    // this run (`AgentCreated`). Static wins over durable wins over authored on a
    // duplicate id.
    let mut registries: Vec<Arc<dyn SkillRegistry>> = Vec::new();
    if !configured.is_empty() {
        registries.push(Arc::new(InMemorySkillRegistry::from_specs(
            configured.iter().cloned(),
        )));
    }
    if let Some(delivered) = delivered {
        let files = delivered
            .into_iter()
            .map(|(id, content)| SkillFile {
                id,
                content,
                dir: None,
            })
            .collect();
        registries.push(Arc::new(SourceSkillRegistry::new(
            Arc::new(SnapshotSkillSource { files }),
            SkillProvenance::Delivered,
        )));
    }
    registries.push(Arc::new(SourceSkillRegistry::new(
        Arc::new(EnvSkillSource {
            env: env.clone(),
            subdir: skills_subdir.to_string(),
        }),
        SkillProvenance::AgentCreated,
    )));
    let registry: Arc<dyn SkillRegistry> = Arc::new(CompositeSkillRegistry::new(registries));

    let activations = PathActivations::new();
    let gate: Arc<dyn ToolGateHook> = Arc::new(RecordingGate::new(base_gate, activations.clone()));
    let list: Arc<dyn RawTool> =
        Arc::new(ListSkillsTool::new(registry.clone()).with_path_activations(activations));
    let fork_runner: Arc<dyn SubagentRunner> = Arc::new(ForkRunner {
        llm,
        model_ref: model_ref.to_string(),
        provider: LocalProvider::new(fork_base),
        sandbox: env,
        reuse_sandbox,
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
    use awaken_provisioning_contract::Sandbox as _;

    #[tokio::test]
    async fn durable_store_snapshot_scans_the_catalog_as_delivered_skill_files() {
        // The host loads a snapshot of the durable catalog (async) and the bridge
        // yields neutral SkillFiles the extension parses — so a skill persisted in the
        // store is offered as Delivered without awaken-ext-skills ever seeing the store.
        use awaken_skill_store::{FsSkillStore, SkillStore};
        let root = std::env::temp_dir().join(format!("awaken-skillsrc-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let store = FsSkillStore::open(&root).unwrap();
        store
            .put("ws", "greet", "---\ndescription: say hi\n---\nHELLO")
            .await
            .unwrap();

        // The host's snapshot → SkillFiles.
        let snapshot = store.list("ws").await.unwrap();
        let files = snapshot
            .into_iter()
            .map(|(id, content)| SkillFile {
                id,
                content,
                dir: None,
            })
            .collect();
        let source = SnapshotSkillSource { files };
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
        let provider = LocalProvider::new(&base);
        let env = Arc::new(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        );

        let registry = SourceSkillRegistry::new(
            Arc::new(EnvSkillSource {
                env: env.clone(),
                subdir: DEFAULT_SKILLS_SUBDIR.to_string(),
            }),
            SkillProvenance::AgentCreated,
        );
        assert!(registry.list().is_empty(), "nothing authored yet");

        let skill_dir = base.join("t").join(DEFAULT_SKILLS_SUBDIR).join("notes");
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

        env.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn a_negotiated_skills_dir_is_scanned_instead_of_the_default() {
        // A hand/agent that authors skills under a non-default dir (its
        // `plugin_config.skills_dir`) is discovered there — the dir is not hardcoded.
        let base = std::env::temp_dir().join(format!("awaken-skilldir-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let env = Arc::new(
            LocalProvider::new(&base)
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        );
        let registry = SourceSkillRegistry::new(
            Arc::new(EnvSkillSource {
                env: env.clone(),
                subdir: "recipes".to_string(),
            }),
            SkillProvenance::AgentCreated,
        );

        // A skill under the DEFAULT `skills/` dir is NOT seen (we negotiated `recipes`).
        let default_dir = base.join("t").join("skills").join("ignored");
        std::fs::create_dir_all(&default_dir).unwrap();
        std::fs::write(default_dir.join("SKILL.md"), "---\ndescription: no\n---\n").unwrap();
        assert!(
            registry.get("ignored").is_none(),
            "the default dir is not scanned"
        );

        // A skill under the negotiated `recipes/` dir IS discovered, tagged by that dir.
        let recipe_dir = base.join("t").join("recipes").join("bake");
        std::fs::create_dir_all(&recipe_dir).unwrap();
        std::fs::write(
            recipe_dir.join("SKILL.md"),
            "---\ndescription: bake\n---\nmix",
        )
        .unwrap();
        let found = registry.get("bake").expect("skill in the negotiated dir");
        assert_eq!(found.description, "bake");
        assert_eq!(found.dir.as_deref(), Some("recipes/bake"));

        env.dispose().await.unwrap();
    }
}
