//! Runtime Skill catalog and execution setup.
//!
//! Skills bridge three things only the host sees together: the sandbox
//! environment (to discover workspace skills and resolve `${SKILL_DIR}`), the
//! sub-run capability (for `context: fork`), and the base permission gate (to
//! observe touched paths). Everything skill-*behavioral* lives in
//! `awaken-ext-skills`; this module builds one registry projection and, only for
//! legacy/direct callers, adapts that same registry to the two semantic tools.

use std::path::PathBuf;
use std::sync::Arc;

use awaken_ext_builtin_tools::{AUXILIARY_AGENT, AuxiliaryAgentInput};
use awaken_ext_skills::{
    ActiveSkillTools, CompositeSkillRegistry, FixedSkillRegistry, ListSkillsTool, PathActivations,
    RecordingGate, SkillAllowedToolsGate, SkillEnvironment, SkillFile, SkillProvenance,
    SkillRegistry, SkillSource, SkillSpec, SkillTool, SourceSkillRegistry,
};
use awaken_resource_contract::SkillVersion;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::permission::ToolGateHook;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use awaken_sandbox_local::LocalProvider;

/// The default workspace subdir the agent authors skills under, scanned live so a
/// skill written this run is discovered (ADR-0036 D8). A hand/agent definition can
/// negotiate a different dir via its `plugin_config.skills_dir`; this is the fallback.
pub(crate) const DEFAULT_SKILLS_SUBDIR: &str = "skills";
pub(crate) const MANAGED_SKILLS_SUBDIR: &str = ".claude/skills";
pub(crate) const DELIVERED_SKILLS_SUBDIR: &str =
    awaken_provisioning_contract::WorkspaceLayout::DELIVERED_SKILLS_SUBDIR;

/// Placement policy for Skill `context: fork` auxiliary Runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkillForkPlacement {
    /// Execute in the Session-owned environment.
    #[default]
    SharedSession,
    /// Provision a new isolated environment for the auxiliary Run.
    FreshIsolation,
}

/// Bridges the sandbox [`LocalSandbox`] to the [`SkillSource`] port: scans the
/// workspace skill dir live, returning neutral file data. The host owns this bridge
/// so `awaken-ext-skills` stays sandbox-unaware and the root stays hidden.
struct EnvSkillSource {
    env: Arc<crate::session_environment::SessionEnvironment>,
    subdir: String,
}

impl SkillSource for EnvSkillSource {
    fn scan(&self) -> Vec<SkillFile> {
        self.env
            .scan_skill_dir(&self.subdir)
            .unwrap_or_default()
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
/// [`SkillStore`]: awaken_resource_contract::SkillStore
struct SnapshotSkillSource {
    files: Vec<SkillFile>,
}

fn snapshot_repository_skill_files(
    env: &crate::session_environment::SessionEnvironment,
    roots: &[String],
) -> Vec<SkillFile> {
    roots
        .iter()
        .flat_map(|root| {
            let visible_root = if root.starts_with("workspace/") {
                format!("/{root}")
            } else {
                root.clone()
            };
            env.scan_skill_dir(root)
                .unwrap_or_default()
                .into_iter()
                .map(move |file| SkillFile {
                    // The path qualifies identity so same-named Skills from
                    // multiple repositories remain independently visible.
                    id: format!("repository:{root}:{}", file.id),
                    content: file.content,
                    // Preserve the frozen logical mount spelling and restore a
                    // leading slash only when it is already workspace-qualified.
                    // Legacy relative mounts remain relative until the shared
                    // cross-provider Agent-visible path authority normalizes
                    // them; do not duplicate that mapping in the Skill layer.
                    dir: Some(format!("{visible_root}/{}", file.id)),
                })
        })
        .collect()
}

pub(crate) fn requires_filesystem(version: &SkillVersion, content: &str) -> bool {
    let declared = awaken_ext_skills::parse_skill_md(version.skill_id.to_string(), content);
    declared.environment == SkillEnvironment::Filesystem
        || version
            .files
            .iter()
            .any(|file| file.path != "SKILL.md" && !file.path.ends_with("/SKILL.md"))
}

pub(crate) fn version_requires_environment(version: &SkillVersion) -> bool {
    let Some(content) = version
        .files
        .iter()
        .find(|file| file.path == "SKILL.md")
        .and_then(|file| String::from_utf8(file.content.clone()).ok())
    else {
        return true;
    };
    let spec = awaken_ext_skills::parse_skill_md(version.skill_id.to_string(), &content);
    requires_filesystem(version, &content)
        || spec.context != awaken_ext_skills::SkillContext::Inline
}

impl SkillSource for SnapshotSkillSource {
    fn scan(&self) -> Vec<SkillFile> {
        self.files.clone()
    }
}

/// Runs a `context: fork` skill as an Agent with the capabilities declared by its
/// config, returning its reply. Placement is explicit and defaults to the
/// Session-owned environment. Implements the neutral
/// ordinary Agent-backed tool — the same generic capability used by the goal
/// judge and compactor.
struct ForkAgentTool {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: LocalProvider,
    /// The parent agent's sandbox, shared with the fork by default.
    sandbox: Arc<crate::session_environment::SessionEnvironment>,
    placement: SkillForkPlacement,
    execution: Arc<crate::store::HostCommit>,
}

#[async_trait::async_trait]
impl RawTool for ForkAgentTool {
    fn id(&self) -> &str {
        AUXILIARY_AGENT
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let request: AuxiliaryAgentInput =
            awaken_runtime_contract::tool::parse_tool_args(call.arguments)?;
        // The skill id names the sub-run; the seed is the resolved skill body. Skill
        // activation is out-of-band housekeeping, so its usage stays isolated (this
        // port surfaces only the reply text).
        let name = format!("skill-{}", request.agent_id);
        let sandbox = match self.placement {
            SkillForkPlacement::SharedSession => {
                crate::agent_runner::AgentRunSandbox::Shared(self.sandbox.as_ref())
            }
            SkillForkPlacement::FreshIsolation => {
                crate::agent_runner::AgentRunSandbox::Fresh(&self.provider)
            }
        };
        let delegates = std::collections::HashSet::new();
        crate::agent_runner::run_agent(
            self.llm.clone(),
            crate::agent_runner::AgentExecution {
                agent_id: &request.agent_id,
                model_ref: &self.model_ref,
                delegates: &delegates,
                run_delegation: None,
                context: Some(
                    awaken_runtime_contract::RuntimeRunContext::new()
                        .with_commit(self.execution.clone())
                        .with_reader(self.execution.clone()),
                ),
                #[cfg(test)]
                scheduler: None,
                #[cfg(test)]
                toolsets: &[],
            },
            sandbox,
            &name,
            request.seed,
            None,
        )
        .await
        .map(|(text, _usage)| ToolOutput::ok(call.call_id, text))
        .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

/// A compatibility projection of one already-built registry onto the legacy
/// semantic Skill tools. Managed Sessions never construct this adapter: their
/// selected, pinned bytes are disclosed only through the materialized filesystem.
pub(crate) struct SemanticSkillAdapter {
    pub descriptors: Vec<ToolDescriptor>,
    pub list_tool: Arc<dyn RawTool>,
    pub activate_tool: Arc<dyn RawTool>,
    pub gate: Arc<dyn ToolGateHook>,
}

/// Build the one Skill registry for a thread, or `None` when no Skills are
/// offered. Filesystem delivery materializes the exact same registry inputs that
/// its progressive-disclosure prompt later reads; it does not construct a second
/// semantic executor.
pub(crate) async fn build_skill_registry(
    configured: &[SkillSpec],
    external_registries: Vec<Arc<dyn SkillRegistry>>,
    delivered: Option<Vec<SkillVersion>>,
    env: Option<Arc<crate::session_environment::SessionEnvironment>>,
    authored_skills_subdir: &str,
    skill_source_roots: Option<&[String]>,
    filesystem_delivery: bool,
) -> Result<Option<Arc<dyn SkillRegistry>>, String> {
    if let (Some(env), Some(roots)) = (&env, skill_source_roots) {
        // `Some([])` is the direct live-authored compatibility source. An
        // explicit root list is instead a closed startup snapshot; Managed
        // supplies only realized repository roots and must not implicitly add
        // an unmounted workspace root.
        if roots.is_empty() {
            env.register_skill_dir(authored_skills_subdir);
        }
        for root in roots {
            env.register_skill_dir(root);
        }
        if let Err(error) = env.refresh_skills().await {
            tracing::warn!(error = %error, "failed to seed container skill catalog");
        }
    }
    // Repository discovery is frozen once with the Session environment. A
    // later commit or in-sandbox write cannot mutate the announced catalog;
    // the next Session receives a new snapshot from its own checkout.
    let repository_files = env.as_ref().map_or_else(Vec::new, |env| {
        skill_source_roots.map_or_else(Vec::new, |roots| {
            snapshot_repository_skill_files(env, roots)
        })
    });
    // Store availability is not a capability grant. Build a projection only
    // when this exact Session has a static, external, delivered, or repository
    // Skill. A later direct Run may reload its catalog; an empty store never
    // creates an ambient Skill surface by itself.
    if configured.is_empty()
        && external_registries.is_empty()
        && delivered.as_ref().is_none_or(Vec::is_empty)
        && repository_files.is_empty()
    {
        return Ok(None);
    }
    if let Some(skill) = configured
        .iter()
        .find(|skill| skill.environment == SkillEnvironment::Filesystem && skill.dir.is_none())
        .filter(|_| !filesystem_delivery)
    {
        return Err(format!(
            "filesystem Skill `{}` has no materialized directory",
            skill.id
        ));
    }
    // `.skills` is one complete runtime-owned projection. Clear it before a
    // rebuild that carries either frozen bundles or config-only Skills, then
    // repopulate every selected Skill through this same projection path.
    if (delivered.is_some() || (filesystem_delivery && !configured.is_empty()))
        && let Some(env) = env.as_ref()
    {
        env.remove_projection_path(DELIVERED_SKILLS_SUBDIR)
            .await
            .map_err(|error| error.to_string())?;
    }
    let mut configured = configured.to_vec();
    if filesystem_delivery {
        for skill in &mut configured {
            if skill.dir.is_some() {
                continue;
            }
            let directory = format!(
                "{DELIVERED_SKILLS_SUBDIR}/{}",
                awaken_resource_contract::skill_stem(&skill.id)
            );
            let name = serde_json::to_string(&skill.name)
                .map_err(|error| format!("serialize Skill name: {error}"))?;
            let description = serde_json::to_string(&skill.description)
                .map_err(|error| format!("serialize Skill description: {error}"))?;
            let content = format!(
                "---\nname: {name}\ndescription: {description}\n---\n{}",
                skill.body
            );
            if let Some(env) = env.as_ref() {
                env.materialize_read_only_tree(
                    &directory,
                    &[("SKILL.md".to_string(), content.into_bytes(), false)],
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            skill.dir = Some(directory);
        }
    }
    // Registry precedence follows the admitted sources: configured direct
    // compatibility specs, frozen delivered versions, external direct adapters,
    // then one frozen Environment-root snapshot. Managed supplies only resolved
    // attached bytes plus realized repository roots; it never supplies the
    // direct sources or a live authored root.
    let mut registries: Vec<Arc<dyn SkillRegistry>> = Vec::new();
    if !configured.is_empty() {
        registries.push(Arc::new(FixedSkillRegistry::from_specs(configured)));
    }
    if let Some(delivered) = delivered {
        let mut files = Vec::with_capacity(delivered.len());
        for version in delivered {
            if awaken_resource_contract::skill_bundle_sha256(&version.files)
                != version.bundle_sha256
            {
                return Err(format!(
                    "Skill {} version {} bundle hash mismatch",
                    version.skill_id, version.version
                ));
            }
            let content = version
                .skill_md()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    format!(
                        "Skill {} version {} has no UTF-8 SKILL.md",
                        version.skill_id, version.version
                    )
                })?
                .to_string();
            // For the direct semantic adapter, a SKILL.md-only bundle stays in
            // the host snapshot unless it declares a filesystem requirement.
            // Managed delivery materializes every selected bundle because its
            // only disclosure contract is the advertised path. Any supporting
            // file also makes materialization objective regardless of metadata.
            let directory =
                (filesystem_delivery || requires_filesystem(&version, &content)).then(|| {
                    format!(
                        "{DELIVERED_SKILLS_SUBDIR}/{}",
                        awaken_resource_contract::skill_stem(version.skill_id.as_str())
                    )
                });
            if let Some(directory) = &directory {
                if let Some(env) = env.as_ref() {
                    let materialized = version
                        .files
                        .iter()
                        .map(|file| (file.path.clone(), file.content.clone(), file.executable))
                        .collect::<Vec<_>>();
                    env.materialize_read_only_tree(directory, &materialized)
                        .await
                        .map_err(|error| error.to_string())?;
                } else if !filesystem_delivery {
                    return Err(format!(
                        "filesystem Skill `{}` requires a materialized Session environment",
                        version.skill_id
                    ));
                }
            }
            files.push(SkillFile {
                id: version.skill_id.to_string(),
                content,
                dir: directory,
            });
        }
        registries.push(Arc::new(SourceSkillRegistry::new(
            Arc::new(SnapshotSkillSource { files }),
            SkillProvenance::Delivered,
        )));
    }
    registries.extend(external_registries);
    if !repository_files.is_empty() {
        registries.push(Arc::new(SourceSkillRegistry::new(
            Arc::new(SnapshotSkillSource {
                files: repository_files,
            }),
            SkillProvenance::Repository,
        )));
    }
    if skill_source_roots.is_some_and(<[String]>::is_empty)
        && let Some(env) = &env
    {
        registries.push(Arc::new(SourceSkillRegistry::new(
            Arc::new(EnvSkillSource {
                env: env.clone(),
                subdir: authored_skills_subdir.to_string(),
            }),
            SkillProvenance::AgentCreated,
        )));
    }
    Ok(Some(Arc::new(CompositeSkillRegistry::new(registries))))
}

/// Adapt one canonical registry to the legacy/direct semantic tool contract.
/// `base_gate` is wrapped so conditional (`paths`) Skills surface on file touch;
/// `fork_base` is the sub-agent sandbox base for `context: fork` Skills.
#[allow(clippy::too_many_arguments)]
pub(crate) fn semantic_skill_adapter(
    registry: Arc<dyn SkillRegistry>,
    env: Option<Arc<crate::session_environment::SessionEnvironment>>,
    llm: Arc<dyn LlmExecutor>,
    model_ref: &str,
    session_id: &str,
    base_gate: Arc<dyn ToolGateHook>,
    fork_base: PathBuf,
    placement: SkillForkPlacement,
    execution: Arc<crate::store::HostCommit>,
) -> SemanticSkillAdapter {
    let activations = PathActivations::new();
    let active_tools = ActiveSkillTools::new();
    let recording: Arc<dyn ToolGateHook> =
        Arc::new(RecordingGate::new(base_gate, activations.clone()));
    let gate: Arc<dyn ToolGateHook> =
        Arc::new(SkillAllowedToolsGate::new(recording, active_tools.clone()));
    let list: Arc<dyn RawTool> =
        Arc::new(ListSkillsTool::new(registry.clone()).with_path_activations(activations));
    let mut activate = SkillTool::new(registry.clone())
        .with_session_id(session_id)
        .with_active_tools(active_tools);
    if let Some(env) = env {
        let agent_tool: Arc<dyn RawTool> = Arc::new(ForkAgentTool {
            llm,
            model_ref: model_ref.to_string(),
            provider: LocalProvider::new(fork_base),
            sandbox: env,
            placement,
            execution,
        });
        activate = activate.with_agent_tool(agent_tool);
    }
    let activate: Arc<dyn RawTool> = Arc::new(activate);

    SemanticSkillAdapter {
        descriptors: vec![list_skills_descriptor(), skill_descriptor()],
        list_tool: list,
        activate_tool: activate,
        gate,
    }
}

/// Anthropic-compatible progressive-disclosure metadata. The prompt carries
/// only name, description, and the exact `SKILL.md` path; the model reads full
/// instructions with ordinary file tools when the Skill is relevant.
pub(crate) fn filesystem_skill_prompt(registry: &dyn SkillRegistry) -> Option<String> {
    let entries = registry
        .list()
        .into_iter()
        .filter(|skill| skill.model_invocable)
        .filter_map(|skill| {
            skill
                .dir
                .map(|dir| format!("- {}: {} (`{dir}/SKILL.md`)", skill.name, skill.description))
        })
        .collect::<Vec<_>>();
    (!entries.is_empty()).then(|| {
        format!(
            "Available Skills are listed below. When a Skill is relevant, read its `SKILL.md` from the given path before acting.\n{}",
            entries.join("\n")
        )
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

    fn version_with(files: Vec<awaken_skill_store::SkillBundleFile>) -> SkillVersion {
        SkillVersion {
            id: "skver-test-1".into(),
            skill_id: "test".into(),
            version: 1,
            name: "test".into(),
            description: String::new(),
            directory: "/skills/test".into(),
            bundle_sha256: awaken_skill_store::bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        }
    }

    #[test]
    fn instruction_only_and_filesystem_bundles_select_the_minimum_substrate() {
        use awaken_skill_store::SkillBundleFile;
        let pure_body = "---\ndescription: think\n---\nThink carefully.";
        let pure = version_with(vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: pure_body.as_bytes().to_vec(),
            executable: false,
        }]);
        assert!(!requires_filesystem(&pure, pure_body));

        let declared_body = "---\nenvironment: filesystem\n---\nRead the workspace.";
        let declared = version_with(vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: declared_body.as_bytes().to_vec(),
            executable: false,
        }]);
        assert!(requires_filesystem(&declared, declared_body));

        let bundled = version_with(vec![
            SkillBundleFile {
                path: "SKILL.md".into(),
                content: pure_body.as_bytes().to_vec(),
                executable: false,
            },
            SkillBundleFile {
                path: "references/guide.md".into(),
                content: b"guide".to_vec(),
                executable: false,
            },
        ]);
        assert!(
            requires_filesystem(&bundled, pure_body),
            "supporting files override instruction-only metadata"
        );
    }

    #[tokio::test]
    async fn durable_store_snapshot_scans_the_catalog_as_delivered_skill_files() {
        // The host loads a snapshot of the durable catalog (async) and the bridge
        // yields neutral SkillFiles the extension parses — so a skill persisted in the
        // store is offered as Delivered without awaken-ext-skills ever seeing the store.
        use awaken_skill_store::{
            FsSkillStore, SkillBundleFile, SkillDefinition, SkillStore, SkillVersion, bundle_sha256,
        };
        let root = std::env::temp_dir().join(format!("awaken-skillsrc-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let store = FsSkillStore::open(&root).unwrap();
        let body = b"---\ndescription: say hi\n---\nHELLO".to_vec();
        let bundle = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: body.clone(),
            executable: false,
        }];
        store
            .create(
                SkillDefinition {
                    id: "greet".into(),
                    workspace_id: "ws".into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
                    timestamps: Default::default(),
                },
                SkillVersion {
                    id: "skver-greet-1".into(),
                    skill_id: "greet".into(),
                    version: 1,
                    name: "greet".into(),
                    description: "say hi".into(),
                    directory: "/skills/greet".into(),
                    bundle_sha256: bundle_sha256(&bundle),
                    files: bundle,
                    created_unix_nanos: 0,
                },
            )
            .await
            .unwrap();

        // The host's snapshot → SkillFiles.
        let version = store.version("ws", "greet", 1).await.unwrap().unwrap();
        let files = vec![SkillFile {
            id: version.skill_id.to_string(),
            content: String::from_utf8(version.skill_md().unwrap().to_vec()).unwrap(),
            dir: None,
        }];
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
        let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        ));

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
    async fn managed_registry_excludes_unbound_workspace_skill_files() {
        // Managed repository Skill cause/effect rules R1/R3/R7. C1 exact
        // attached version bytes exist; C2 an unmounted workspace
        // `.claude/skills` file also exists; C3 no repository roots were
        // admitted (no mount or exact `read` denied). E1 only C1 enters the one
        // registry and filesystem prompt. R1=C1+!repository=>E1;
        // R3=C1+C2+C3=>E1; R7 adds host-static/live-catalog/MCP registries as
        // equally unbound inputs, excluded by the Managed caller before this
        // builder (`managed_session_ignores_unbound_host_skill_sources` owns
        // their integration row). Direct live authoring remains owned by
        // `agent_authored_skill_is_discovered_live_from_the_workspace`.
        let base = std::env::temp_dir().join(format!(
            "awaken-managed-skill-authority-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(&base)
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        ));
        let unbound = base.join("t").join(MANAGED_SKILLS_SUBDIR).join("unbound");
        std::fs::create_dir_all(&unbound).unwrap();
        std::fs::write(
            unbound.join("SKILL.md"),
            "---\nname: Unbound\ndescription: must stay hidden\n---\nUNBOUND",
        )
        .unwrap();
        let frozen = version_with(vec![awaken_skill_store::SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: Frozen\ndescription: pinned\n---\nFROZEN".to_vec(),
            executable: false,
        }]);

        let registry = build_skill_registry(
            &[],
            Vec::new(),
            Some(vec![frozen]),
            Some(env.clone()),
            MANAGED_SKILLS_SUBDIR,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("R1/R3/R7 frozen Managed registry");
        let listed = registry.list();
        assert_eq!(listed.len(), 1, "R1/R3/R7 no parallel workspace entry");
        assert_eq!(listed[0].id, "test", "R1 exact frozen id");
        let prompt = filesystem_skill_prompt(registry.as_ref()).expect("R1 prompt");
        assert!(
            prompt.contains("Frozen") && !prompt.contains("Unbound"),
            "R3/R7"
        );

        env.dispose().await.unwrap();
        std::fs::remove_dir_all(base).ok();
    }

    #[tokio::test]
    async fn a_negotiated_skills_dir_is_scanned_instead_of_the_default() {
        // A hand/agent that authors skills under a non-default dir (its
        // `plugin_config.skills_dir`) is discovered there — the dir is not hardcoded.
        let base = std::env::temp_dir().join(format!("awaken-skilldir-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(&base)
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        ));
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

    #[tokio::test]
    async fn repository_skill_snapshot_is_startup_scoped_and_path_qualified() {
        // Managed repository Skill cause/effect rules R2/R4/R5/R6. C1 exact
        // attached bytes and two admitted realized repository roots exist; C2
        // each root has exact `<name>/SKILL.md`; C3 malformed root-level,
        // nested, and wrong-name files coexist; C4 repository files mutate
        // after construction. Effects: E1 attached and repository entries share
        // one Composite registry; E2 only C2 is discovered; E3 same display
        // names retain path-qualified identities; E4 the current registry stays
        // frozen and the next Session snapshot sees changes. Rules:
        // R2=C1+C2=>E1; R4=C2+C3=>E2; R5=same-name=>E3;
        // R6=C4=>current-old+next-new. Repository realization itself is reused;
        // this builder neither clones nor fetches.
        let base = std::env::temp_dir().join(format!(
            "awaken-repository-skill-snapshot-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(&base)
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        ));
        let roots =
            ["workspace/a/.claude/skills", "workspace/b/.claude/skills"].map(str::to_string);
        for (root, body) in roots.iter().zip(["A-v1", "B-v1"]) {
            let directory = base.join("t").join(root).join("shared");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: Shared\ndescription: shared\n---\n{body}"),
            )
            .unwrap();
            std::fs::write(base.join("t").join(root).join("SKILL.md"), "ROOT").unwrap();
            let nested = base.join("t").join(root).join("nested/too-deep");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(nested.join("SKILL.md"), "NESTED").unwrap();
            let wrong_name = base.join("t").join(root).join("wrong-name");
            std::fs::create_dir_all(&wrong_name).unwrap();
            std::fs::write(wrong_name.join("skill.md"), "WRONG").unwrap();
        }

        let attached = version_with(vec![awaken_skill_store::SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: Shared\ndescription: shared\n---\nATTACHED".to_vec(),
            executable: false,
        }]);
        let frozen = build_skill_registry(
            &[],
            Vec::new(),
            Some(vec![attached.clone()]),
            Some(env.clone()),
            MANAGED_SKILLS_SUBDIR,
            Some(&roots),
            true,
        )
        .await
        .unwrap()
        .expect("R2 one attached-plus-repository registry");
        let initial = frozen.list();
        assert_eq!(initial.len(), 3, "R2/R4 only admitted exact entries");
        assert!(initial.iter().all(|skill| skill.name == "Shared"));
        let repository = initial
            .iter()
            .filter(|skill| skill.provenance == SkillProvenance::Repository)
            .collect::<Vec<_>>();
        assert_eq!(repository.len(), 2, "R2/R4 two exact repository entries");
        assert_ne!(
            repository[0].id, repository[1].id,
            "R5 path-qualified identity"
        );
        assert_ne!(
            repository[0].dir, repository[1].dir,
            "R5 distinct sandbox paths"
        );
        let prompt = filesystem_skill_prompt(frozen.as_ref()).expect("R2 prompt metadata");
        assert_eq!(
            prompt.matches("- Shared:").count(),
            3,
            "R2/R5 attached and repositories coexist"
        );
        assert!(prompt.contains(".skills/test/SKILL.md"));
        assert!(prompt.contains("/workspace/a/.claude/skills/shared/SKILL.md"));
        assert!(prompt.contains("/workspace/b/.claude/skills/shared/SKILL.md"));
        assert!(
            !prompt.contains("ATTACHED") && !prompt.contains("A-v1") && !prompt.contains("B-v1"),
            "R2 prompt exposes metadata and paths, not instruction bodies"
        );

        let a = base.join("t/workspace/a/.claude/skills/shared/SKILL.md");
        std::fs::write(&a, "---\nname: Shared\ndescription: shared\n---\nA-v2").unwrap();
        let late = base.join("t/workspace/a/.claude/skills/late");
        std::fs::create_dir_all(&late).unwrap();
        std::fs::write(
            late.join("SKILL.md"),
            "---\nname: Late\ndescription: late\n---\nLATE",
        )
        .unwrap();
        let still_frozen = frozen.list();
        assert_eq!(still_frozen.len(), 3, "R6 no mid-Session discovery");
        assert!(still_frozen.iter().any(|skill| skill.body.contains("A-v1")));
        assert!(
            still_frozen
                .iter()
                .all(|skill| !skill.body.contains("A-v2"))
        );

        let next = build_skill_registry(
            &[],
            Vec::new(),
            Some(vec![attached]),
            Some(env.clone()),
            MANAGED_SKILLS_SUBDIR,
            Some(&roots),
            true,
        )
        .await
        .unwrap()
        .expect("R6 next Session registry")
        .list();
        assert_eq!(next.len(), 4, "R6 new Session snapshot");
        assert!(next.iter().any(|skill| skill.body.contains("A-v2")));
        assert!(next.iter().any(|skill| skill.name == "Late"));

        env.dispose().await.unwrap();
        std::fs::remove_dir_all(base).ok();
    }

    #[tokio::test]
    #[should_panic(
        expected = "KNOWN GAP: a repository Skill scan failure must not become an empty catalog"
    )]
    async fn repository_skill_snapshot_propagates_scan_failure() {
        /* Temporary expected-failure regression; remove `should_panic` when
         * the production fix lands.
         *
         * Cause/effect graph: C1 a fixed Managed repository root is admitted;
         * C2 the root is absent, readable, or returns a structural/UTF-8 scan
         * error. Effects: E1 absence is a legitimate empty snapshot; E2 valid
         * files use the existing path-qualified snapshot path; E3 scan errors
         * abort construction before any registry or prompt is published.
         * Decision table: SS1 C1+absent => E1; SS2 C1+readable => E2 (owned by
         * `repository_skill_snapshot_is_startup_scoped_and_path_qualified`);
         * SS3 C1+scan-error => E3. This test owns SS1/SS3 and reuses the
         * authoritative sandbox scanner and registry builder instead of
         * introducing a second discovery fake. */
        let base = tempfile::tempdir().expect("SS1/SS3 temporary repository root");
        let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(base.path())
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        ));
        let root = "workspace/repository/.claude/skills".to_owned();

        let missing = build_skill_registry(
            &[],
            Vec::new(),
            None,
            Some(env.clone()),
            MANAGED_SKILLS_SUBDIR,
            Some(std::slice::from_ref(&root)),
            true,
        )
        .await
        .expect("SS1 an absent repository Skill root is an empty snapshot");
        assert!(missing.is_none(), "SS1 no ambient Skill surface");

        let broken = base.path().join("t").join(&root).join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("SKILL.md"), [0xff, 0xfe]).unwrap();
        let error = match build_skill_registry(
            &[],
            Vec::new(),
            None,
            Some(env.clone()),
            MANAGED_SKILLS_SUBDIR,
            Some(std::slice::from_ref(&root)),
            true,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!(
                "KNOWN GAP: a repository Skill scan failure must not become an empty catalog"
            ),
        };
        assert!(error.contains("UTF-8"), "SS3: {error}");

        env.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn attached_integrity_failure_cannot_fall_back_to_a_repository_skill() {
        // Managed repository Skill cause/effect rule R8. C1 an attached binding
        // supplies tampered version bytes; C2 an admitted repository contains a
        // valid same-named Skill. E1 construction fails on C1 before any prompt
        // is published; C2 cannot replace the attached binding/version/hash
        // authority. R8=C1+C2=>E1.
        let base = std::env::temp_dir().join(format!(
            "awaken-managed-skill-integrity-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            LocalProvider::new(&base)
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
                .await
                .unwrap(),
        ));
        let root = "workspace/repo/.claude/skills".to_string();
        let repository_skill = base.join("t").join(&root).join("test");
        std::fs::create_dir_all(&repository_skill).unwrap();
        std::fs::write(
            repository_skill.join("SKILL.md"),
            "---\nname: test\ndescription: repository\n---\nVALID",
        )
        .unwrap();
        let mut tampered = version_with(vec![awaken_skill_store::SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: test\ndescription: attached\n---\nTAMPERED".to_vec(),
            executable: false,
        }]);
        tampered.bundle_sha256 = "sha256:wrong".into();

        let error = match build_skill_registry(
            &[],
            Vec::new(),
            Some(vec![tampered]),
            Some(env.clone()),
            MANAGED_SKILLS_SUBDIR,
            Some(&[root]),
            true,
        )
        .await
        {
            Ok(_) => panic!("R8 attached corruption must fail closed"),
            Err(error) => error,
        };
        assert!(error.contains("bundle hash mismatch"), "R8/E1: {error}");

        env.dispose().await.unwrap();
        std::fs::remove_dir_all(base).ok();
    }
}
