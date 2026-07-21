//! Session provisioning + capability advertisement on [`SharedHost`]: the sandbox
//! spec (with staged resource mounts, ADR-0038), the per-thread resource staging +
//! blob store accessor, and the tool/skill/delegate sets advertised on a managed
//! session. Split out of `host.rs` to keep that file under the length limit; these
//! are the same `SharedHost` (fields are `pub(crate)`).

use std::sync::Arc;

use crate::host::SharedHost;
use awaken_file_store::FileStore;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::resolved::ToolDescriptor;

/// The sandbox-absolute outputs dir (must be absolute for `prepare_environment`);
/// resolved under the root to `<root>/outputs`, which `list_files("outputs")` reads.
const OUTPUTS_PATH: &str = "/outputs";

/// A bare Workdir spec for an ephemeral sub-run sandbox (judge / delegate / compact /
/// skill fork): scoped to the thread, no staged resource mounts, host-shared network.
pub(crate) fn agent_run_sandbox_spec(thread: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: thread.to_string(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: OUTPUTS_PATH.to_string(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

/// A thread's staged neutral mount requirements plus the prompt fragments derived
/// from the same effective Session inputs.
#[derive(Default, Clone)]
pub(crate) struct StagedResources {
    pub mounts: Vec<pc::MountRequirement>,
    pub prompts: Vec<String>,
    /// github_repository resources (ADR-0038). Provisioned by a host-side `git clone`
    /// after the environment is created (not a byte mount) and pushed back on harvest.
    pub repos: Vec<RepoStage>,
}

/// A staged github_repository: cloned host-side into the jailed `logical` path and
/// pushed back on harvest. `credential` is materialized at the runtime injection
/// seam, used only on host transports, and never enters the sandbox jail.
#[derive(Clone)]
pub(crate) struct RepoStage {
    pub logical: String,
    pub url: String,
    pub git_ref: Option<String>,
    pub credential: Option<awaken_agent_contract::RedactedString>,
    pub access: pc::MountAccess,
}

impl SharedHost {
    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    pub(crate) fn sandbox_spec(&self, thread: &str) -> pc::SandboxSpec {
        let mounts = self
            .thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|staged| staged.mounts.clone())
            .unwrap_or_default();
        // Egress denial is a Workdir-tier bwrap convenience (not admission-gated
        // network isolation, which this tier cannot enforce), so it rides `extra`.
        let extra = self
            .thread_egress
            .denies(thread)
            .then(|| serde_json::json!({ "deny_egress": true }));
        let base = pc::SandboxSpec {
            scope: thread.to_string(),
            isolation: pc::IsolationClass::Workdir,
            mounts,
            env: Vec::new(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: OUTPUTS_PATH.to_string(),
            limits: pc::ResourceLimits::default(),
            lease_ttl_secs: None,
            extra,
        };
        // Overlay the session environment's `config.sandbox` (isolation/network/limits),
        // so a UI-authored sandbox shapes the native bash-tool jail too — the SAME
        // override the ACP channel source applies (shared `thread_sandbox` handle).
        self.thread_sandbox.apply(thread, base)
    }

    /// A shared clone of the thread-resources registry (like [`Self::thread_egress`]),
    /// so a sandboxed ACP channel source reads the SAME staged mounts the native
    /// `sandbox_spec` does and carries them into the bwrap/container sandbox. `pub` so a
    /// composition root (e.g. a scenario host wiring a container-tier ACP source) can
    /// pass it to [`crate::build_acp_channel_source`], symmetric with `thread_egress`.
    pub fn thread_resources_handle(&self) -> crate::sandbox_source::ThreadResources {
        crate::sandbox_source::ThreadResources::new(self.thread_resources.clone())
    }

    /// Stage a thread's resources (mounts + prompt fragments); consumed by
    /// `sandbox_spec` and injected into the run's system prompt. From `prepare_session`.
    /// REPLACES the thread's set (correct at create time, before any first turn).
    pub(crate) fn register_thread_resources(&self, thread: &str, staged: StagedResources) {
        self.thread_resources
            .lock()
            .unwrap()
            .insert(thread.to_string(), staged);
    }

    /// Replace only the repository-derived MCP projections, preserving authored
    /// and Session-inline MCP servers owned by the independent MCP plane.
    pub(crate) fn replace_thread_repository_mcp(
        &self,
        thread: &str,
        repository_mcp: Vec<crate::host::PreparedMcpServer>,
    ) {
        let mut all = self.thread_mcp.lock().unwrap();
        let servers = all.entry(thread.to_string()).or_default();
        servers.retain(|server| !server.name.starts_with("github:"));
        servers.extend(repository_mcp);
    }

    /// The github_repository stages queued for `thread` (test-only observability: a
    /// repo is cloned host-side, not a byte mount, so it is absent from `sandbox_spec`).
    #[cfg(test)]
    pub(crate) fn thread_repos(&self, thread: &str) -> Vec<RepoStage> {
        self.thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|s| s.repos.clone())
            .unwrap_or_default()
    }

    /// The MCP servers staged for `thread` (test-only observability, mirrors
    /// [`Self::thread_repos`]): the set `register_thread_mcp` recorded, including any
    /// `github:<logical>` server bridged from a github_repository resource.
    #[cfg(test)]
    pub(crate) fn thread_mcp(&self, thread: &str) -> Vec<crate::host::PreparedMcpServer> {
        self.thread_mcp
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .unwrap_or_default()
    }

    /// The prompt fragments staged for `thread`'s bound resources (ADR-0038 A3a).
    pub(crate) fn thread_resource_prompts(&self, thread: &str) -> Vec<String> {
        self.thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|s| s.prompts.clone())
            .unwrap_or_default()
    }

    /// The content-addressed blob store (Files API, file-resource mounts, artifacts).
    pub fn file_store(&self) -> Arc<dyn FileStore> {
        self.file_store.clone()
    }

    pub fn grant_file(&self, workspace: &str, id: &str) {
        self.resource_ownership.grant("file", id, workspace);
    }

    pub fn owns_file(&self, workspace: &str, id: &str) -> bool {
        self.resource_ownership.owns("file", id, workspace)
    }

    pub fn revoke_file(&self, workspace: &str, id: &str) -> bool {
        self.resource_ownership.revoke("file", id, workspace)
    }

    pub fn file_has_any_owner(&self, id: &str) -> bool {
        self.resource_ownership.has_any_owner("file", id)
    }

    /// Clone a thread's staged github_repository resources into its freshly-created
    /// environment (ADR-0038). Runs after `provider.create`, host-side, so the token
    /// authenticates the clone transport without ever entering the jail. Fail-closed:
    /// a clone error surfaces so the session doesn't run believing a repo mounted.
    pub(crate) fn provision_thread_repos(
        &self,
        thread: &str,
        sandbox: &awaken_sandbox_local::LocalSandbox,
    ) -> Result<(), crate::host::HostError> {
        let repos = self
            .thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|s| s.repos.clone())
            .unwrap_or_default();
        for repo in repos {
            sandbox
                .provision_repo(
                    &repo.logical,
                    &repo.url,
                    repo.git_ref.as_deref(),
                    repo.credential.as_ref().map(|value| value.expose_secret()),
                )
                .map_err(|e| crate::host::HostError::internal(e.to_string()))?;
        }
        Ok(())
    }

    /// Push a thread's github_repository commits back to their remotes (ADR-0038
    /// write-back): host-side `push` with the held
    /// token. The AGENT authors the commits (its own message + identity) in the jail; the
    /// host only pushes them (it alone holds the token) — it never fabricates a commit.
    ///
    /// A repo whose remote ops the agent owns through an injected GitHub MCP server (the
    /// Managed Agents model — branch/commit/push/PR via MCP tools) is SKIPPED here: pushing
    /// host-side too would double-write or conflict with the agent's own pushes. Host-push
    /// remains only the fallback for a repo with no GitHub MCP (e.g. a non-MCP CLI). A no-op
    /// for a thread with no repos, no live env, or nothing the agent committed. Best-effort.
    pub async fn harvest_thread_repo(&self, thread: &str) {
        let (env, repos) = {
            let sessions = self.sessions.lock().await;
            let env = sessions.get(thread).map(|ctx| ctx.env.clone());
            let repos = self
                .thread_resources
                .lock()
                .unwrap()
                .get(thread)
                .map(|s| s.repos.clone())
                .unwrap_or_default();
            (env, repos)
        };
        let Some(env) = env else {
            return;
        };
        // Repos the agent pushes itself via an injected `github:<logical>` MCP server.
        let mcp_owned: std::collections::HashSet<String> = self
            .thread_mcp
            .lock()
            .unwrap()
            .get(thread)
            .into_iter()
            .flatten()
            .filter_map(|s| s.name.strip_prefix("github:").map(String::from))
            .collect();
        for repo in repos {
            if repo.access == pc::MountAccess::ReadOnly {
                continue;
            }
            if mcp_owned.contains(&repo.logical) {
                continue;
            }
            let _ = env.push_repo(
                &repo.logical,
                repo.credential.as_ref().map(|value| value.expose_secret()),
            );
        }
    }

    /// Harvest a thread's run-authored skills into the durable catalog (ADR-0036 D6/D8,
    /// scan the workspace skill dir for skills the
    /// agent authored this run — the self-authoring loop a Hermes-style agent runs — and
    /// persist each under its id, so a skill written in this session is delivered to the
    /// next one that opens against the same catalog. A no-op for a thread with no live
    /// environment or a host with no durable skill store (nothing to persist into).
    /// Idempotent: a re-scanned delivered skill puts identical bytes back under the same id.
    pub async fn harvest_thread_skills(&self, thread: &str) {
        if !self.skills.has_store() {
            return;
        }
        let env = {
            let sessions = self.sessions.lock().await;
            sessions.get(thread).map(|ctx| ctx.env.clone())
        };
        let Some(env) = env else {
            return;
        };
        let workspace = self.thread_workspace(thread);
        self.persist_authored_skills(&workspace, env.as_ref()).await;
    }

    /// Scan a live environment's workspace skill dir and persist each authored skill to the
    /// durable catalog. Split from [`harvest_thread_skills`](Self::harvest_thread_skills) so
    /// the scan→store path is testable with a real sandbox, without a full `SessionCtx`.
    async fn persist_authored_skills(
        &self,
        workspace: &str,
        env: &awaken_sandbox_local::LocalSandbox,
    ) {
        for skill in env.scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR) {
            self.skills
                .persist_authored(workspace, &skill.id, &skill.content)
                .await;
        }
    }

    /// Collect a session's output artifacts (ADR-0038): the files the agent wrote
    /// under the environment's `outputs/` dir, each stored into the blob store and
    /// returned as `(content_id, logical_path)`. This is the sandbox→host reverse
    /// channel behind `GET /v1/files?scope_id=<session>`; empty when the session has
    /// no environment or wrote nothing.
    pub async fn session_artifacts(&self, thread: &str) -> Vec<(String, String)> {
        let env = {
            let sessions = self.sessions.lock().await;
            sessions.get(thread).map(|ctx| ctx.env.clone())
        };
        let Some(env) = env else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (path, bytes) in env.list_files("outputs") {
            if let Ok(id) = self.file_store.put(&bytes).await {
                let workspace = self.thread_workspace(thread);
                self.grant_file(&workspace, &id);
                out.push((id, path));
            }
        }
        out
    }

    /// The registered built-in tools advertised on a managed session's agent object:
    /// each hand-tool id and whether its calls require confirmation. Folded into the
    /// public `agent_toolset` by the adapter. Deterministic from host config.
    pub fn builtin_tools(&self) -> Vec<(String, bool)> {
        crate::config::builtin_hand_tools()
    }

    /// The client-executed (custom) tools advertised on a managed session: their
    /// descriptors, so the adapter can shape each as a `custom` tool definition.
    pub fn custom_tools(&self) -> Vec<ToolDescriptor> {
        self.client_tools
            .iter()
            .map(|id| crate::config::client_tool_descriptor(id))
            .collect()
    }

    /// The delegate agent ids (advertised as the agent's `multiagent` roster).
    pub fn delegate_ids(&self) -> Vec<String> {
        self.delegates.ids()
    }
}

/// Gap-1 coverage: the resource-staging registry (`register`/`merge`/`remove`),
/// `sandbox_spec` projection, the `provision_thread_repos` fail-closed contract,
/// and the reverse-channel no-ops when a thread has no live environment. These
/// exercise the host-plane provisioning bookkeeping directly; the wired
/// memory/repo write-back happy paths run in `host::tests` through a real session.
#[cfg(test)]
mod provisioning_registry_tests {
    use super::*;
    use crate::host::SharedHost;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
    use awaken_sandbox_local::LocalProvider;

    /// The logical path a staged resource realizes under, recovered from a projected
    /// pc mount (`.mnt/<logical>`) so the registry assertions stay resource-oriented.
    fn logical_of(m: &pc::MountRequirement) -> &str {
        m.mount_path.strip_prefix(".mnt/").unwrap_or(&m.mount_path)
    }

    /// Whether a projected Workdir spec denies egress (carried on the opaque `extra`).
    fn denies(spec: &pc::SandboxSpec) -> bool {
        spec.extra
            .as_ref()
            .and_then(|v| v.get("deny_egress"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    struct NoLlm;
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            unreachable!("provisioning bookkeeping never calls the model")
        }
    }

    fn host() -> SharedHost {
        SharedHost::new(Arc::new(NoLlm), "test")
    }

    /// A resource mount realized read-only under `.mnt/<logical>`.
    fn resource_mount(logical: &str) -> pc::MountRequirement {
        pc::MountRequirement {
            mount_id: format!("id-{logical}"),
            source: pc::MountSource::InlineBytes {
                contents: format!("content of {logical}").into_bytes(),
                content_hash: None,
            },
            mount_path: format!(".mnt/{logical}"),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }
    }

    fn repo_stage(logical: &str) -> RepoStage {
        RepoStage {
            logical: logical.to_string(),
            url: "https://example.invalid/x.git".to_string(),
            git_ref: None,
            credential: None,
            access: pc::MountAccess::ReadWrite,
        }
    }

    #[test]
    fn register_replaces_the_threads_staged_set() {
        let host = host();
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("a.md"), resource_mount("b.md")],
                prompts: vec!["first".into()],
                repos: vec![repo_stage("repo-a")],
            },
        );
        // A second register REPLACES (correct at create time, before any first turn).
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("c.md")],
                prompts: vec!["second".into()],
                repos: Vec::new(),
            },
        );

        assert_eq!(host.sandbox_spec("t").mounts.len(), 1, "old mounts dropped");
        assert_eq!(
            host.thread_resource_prompts("t"),
            vec!["second".to_string()]
        );
        assert!(host.thread_repos("t").is_empty(), "old repo dropped");
    }

    #[test]
    fn sandbox_spec_carries_deny_egress_and_the_staged_mounts() {
        let host = host();
        // No registration and no egress: shared network, no mounts.
        let bare = host.sandbox_spec("t");
        assert!(!denies(&bare));
        assert!(bare.mounts.is_empty());

        host.register_thread_egress("t", true);
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("notes.md")],
                ..Default::default()
            },
        );
        let spec = host.sandbox_spec("t");
        assert!(denies(&spec), "the thread's deny-egress policy is carried");
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(logical_of(&spec.mounts[0]), "notes.md");
    }

    #[tokio::test]
    async fn provision_thread_repos_fails_closed_on_an_unsafe_repo_path() {
        // A jail-escaping logical path is rejected by `LocalSandbox::provision_repo`
        // BEFORE any git runs (deterministic, no git binary needed). The fail-closed
        // contract: that SandboxError surfaces as a HostError so a session never
        // starts believing a repo mounted when it did not.
        let tmp = tempfile::tempdir().unwrap();
        let env = LocalProvider::new(tmp.path())
            .create_sandbox(&agent_run_sandbox_spec("s"))
            .await
            .unwrap();
        let host = host();
        host.register_thread_resources(
            "t",
            StagedResources {
                repos: vec![RepoStage {
                    logical: "../escape".into(),
                    url: "https://example.invalid/x.git".into(),
                    git_ref: None,
                    credential: None,
                    access: pc::MountAccess::ReadWrite,
                }],
                ..Default::default()
            },
        );
        let err = host.provision_thread_repos("t", &env);
        assert!(
            err.is_err(),
            "an unsafe repo mount must abort session start"
        );
    }

    #[tokio::test]
    async fn reverse_channels_are_safe_noops_without_a_live_session() {
        let host = host();
        // Stage a repo, but never create a session for the thread: reverse channels
        // must early-return (no live env), not panic. Memory write-through is owned
        // by the MemoryMount guard and therefore has no Host-side reverse channel.
        host.register_thread_resources(
            "t",
            StagedResources {
                repos: vec![repo_stage("r")],
                ..Default::default()
            },
        );
        host.harvest_thread_repo("t").await; // no env → no push
        host.harvest_thread_skills("t").await; // no env / no store → no persist
        assert!(host.session_artifacts("t").await.is_empty());
        assert!(host.session_artifacts("never-seen").await.is_empty());
    }

    #[tokio::test]
    async fn harvest_persists_an_agent_authored_skill_to_the_durable_catalog() {
        // Hermes-style self-authoring (ADR-0036 D6/D8): a skill the agent writes under the
        // workspace this run must be harvested into the durable catalog so the next session
        // delivers it — the skill analogue of memory write-back.
        let dir = std::env::temp_dir().join(format!("awaken-skillharvest-{}", std::process::id()));
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));

        // A real sandbox env with a skill authored under the workspace `skills/` dir.
        let base = dir.join("sbx");
        let env = LocalProvider::new(&base)
            .create_sandbox(&agent_run_sandbox_spec("t"))
            .await
            .unwrap();
        let skill_dir = base.join("t").join("skills").join("notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: authored this run\n---\nremember to hydrate",
        )
        .unwrap();

        // The catalog is empty until the run-authored skill is harvested; after harvest it
        // holds the skill, addressable for delivery to the next session.
        assert!(
            host.skills
                .definitions(host.local_workspace())
                .await
                .is_empty()
        );
        host.persist_authored_skills(host.local_workspace(), &env)
            .await;
        let ids = host
            .skills
            .definitions(host.local_workspace())
            .await
            .into_iter()
            .map(|definition| definition.id)
            .collect::<Vec<_>>();
        assert!(
            ids.iter().any(|id| id.contains("notes")),
            "the authored skill must be persisted to the durable catalog: {ids:?}"
        );

        pc::Sandbox::dispose(&env).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_durable_skill_is_advertised_by_a_resolvable_catalog_id() {
        // The official worker reads `agent.skills[].skill_id` then downloads it — so the
        // advertised id must be a tagged catalog id the `/v1/skills` read paths resolve,
        // never the skill's name (which used to 404). This pins that round-trip.
        let dir = std::env::temp_dir().join(format!("awaken-skillid-{}", std::process::id()));
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));
        host.skills
            .persist_authored(
                host.local_workspace(),
                "Greeter",
                "---\nname: Greeter\ndescription: hi\n---\nsay hi",
            )
            .await;

        let cid = "Greeter".to_string();
        let advertised = host.skills.ids();
        assert!(
            advertised.contains(&cid),
            "advertisement {advertised:?} must offer the stable resource id {cid}"
        );

        let version = host
            .skills
            .cache_snapshot_in(host.local_workspace())
            .into_iter()
            .find(|version| version.skill_id == cid)
            .expect("the advertised resource id must resolve to the Skill version");
        assert!(version.skill_md().unwrap().ends_with(b"say hi"));
        assert!(
            host.skills
                .cache_snapshot_in(host.local_workspace())
                .into_iter()
                .find(|version| version.skill_id == "skill_deadbeefdeadbeef")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
