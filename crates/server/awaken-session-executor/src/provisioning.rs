//! Session provisioning + capability advertisement on [`SharedHost`]: the sandbox
//! spec (with staged resource mounts, ADR-0038), the per-thread resource staging +
//! blob store accessor, and the tool/skill/delegate sets advertised on a managed
//! session. Split out of `host.rs` to keep that file under the length limit; these
//! are the same `SharedHost` (fields are `pub(crate)`).

use std::sync::Arc;

use awaken_file_store::FileStore;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_sandbox_local::{Mount, SandboxSpec};

use crate::host::SharedHost;

/// The single workspace the host addresses its durable skill catalog under. The
/// `SkillStore` port is workspace-scoped (multi-node/multi-tenant-ready); this host
/// is currently single-catalog, so it uses one fixed workspace. Threading a
/// per-session workspace (ADR-0051) is a later tenancy step over the same store.
pub(crate) const HOST_SKILL_WORKSPACE: &str = "default";

/// The single workspace the host addresses its durable memory-store family under —
/// see [`HOST_SKILL_WORKSPACE`] for the tenancy rationale.
pub(crate) const HOST_MEMORY_WORKSPACE: &str = "default";

/// A thread's staged resources (ADR-0038): the legacy [`Mount`]s realized into its
/// sandbox plus the prompt fragments appended to its system prompt. Built by a
/// session's `prepare_session` from the wire `resources[]`.
#[derive(Default, Clone)]
pub(crate) struct StagedResources {
    pub mounts: Vec<Mount>,
    pub prompts: Vec<String>,
    /// The `(memory_store_id, logical_path)` of each read-write memory mount, so
    /// `harvest_thread_memory` can read the realized file back into the store after a
    /// turn (ADR-0038 MemoryStore write-back). Empty for file/repo mounts.
    pub memory_mounts: Vec<(String, String)>,
    /// github_repository resources (ADR-0038). Provisioned by a host-side `git clone`
    /// after the environment is created (not a byte mount) and pushed back on harvest.
    pub repos: Vec<RepoStage>,
}

/// A staged github_repository: cloned host-side into the jailed `logical` path and
/// pushed back on harvest. The `token` is the host-held GitHub PAT, used only on the
/// git transport — it never enters the sandbox jail.
#[derive(Clone)]
pub(crate) struct RepoStage {
    pub logical: String,
    pub url: String,
    pub git_ref: Option<String>,
    pub token: Option<awaken_agent_contract::RedactedString>,
}

impl SharedHost {
    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    pub(crate) fn sandbox_spec(&self, thread: &str) -> SandboxSpec {
        let mut spec = SandboxSpec::new(thread).with_deny_egress(self.thread_egress.denies(thread));
        if let Some(staged) = self.thread_resources.lock().unwrap().get(thread) {
            for mount in &staged.mounts {
                spec = spec.with_mount(mount.clone());
            }
        }
        spec
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

    /// Append one resource's staging to `thread`'s existing set (the live
    /// `resources.add` path). Unlike the create-time replace, this preserves the
    /// resources already staged, so the next rebuilt sandbox carries all of them.
    pub(crate) fn merge_thread_resources(&self, thread: &str, staged: StagedResources) {
        let mut all = self.thread_resources.lock().unwrap();
        let entry = all.entry(thread.to_string()).or_default();
        entry.mounts.extend(staged.mounts);
        entry.prompts.extend(staged.prompts);
        entry.memory_mounts.extend(staged.memory_mounts);
        entry.repos.extend(staged.repos);
    }

    /// Drop one resource from `thread`'s staged set by its realized `logical` path
    /// and its exact prompt fragment (the live `resources.delete` path). The rest
    /// stay staged, so the next rebuilt sandbox carries everything but this one.
    pub(crate) fn remove_thread_resource(&self, thread: &str, logical: &str, prompt: &str) {
        let mut all = self.thread_resources.lock().unwrap();
        if let Some(entry) = all.get_mut(thread) {
            entry.mounts.retain(|m| match m {
                Mount::Resource(rm) => rm.logical_path != logical,
            });
            entry.repos.retain(|r| r.logical != logical);
            entry.memory_mounts.retain(|(_, l)| l != logical);
            entry.prompts.retain(|p| p != prompt);
        }
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

    /// The `(memory_store_id, logical_path)` memory mounts staged for `thread`
    /// (test-only observability, mirrors [`Self::thread_repos`]): a memory mount is
    /// harvested host-side via `harvest_thread_memory`, not carried in `sandbox_spec`.
    #[cfg(test)]
    pub(crate) fn thread_memory_mounts(&self, thread: &str) -> Vec<(String, String)> {
        self.thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|s| s.memory_mounts.clone())
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

    /// Create a new, empty memory store and return its stable id (ADR-0038 MemoryStore).
    /// Unlike a blob id, this id is mutable: a session mounts it read-write and the host
    /// harvests the write back under the same id. Backed by the durable
    /// [`awaken_memory_store::MemoryBlobStore`], so the store (and its id) survive a
    /// process restart when the host runs under a storage dir.
    pub async fn create_memory_store(&self) -> String {
        self.memory_stores
            .create(HOST_MEMORY_WORKSPACE)
            .await
            .expect("create durable memory store")
    }

    /// The current bytes of a memory store; `None` if the id is unknown.
    pub async fn memory_get(&self, id: &str) -> Option<Vec<u8>> {
        self.memory_stores
            .get(HOST_MEMORY_WORKSPACE, id)
            .await
            .unwrap_or(None)
    }

    /// Store (or overwrite) a delivered skill's `SKILL.md` `content` under `id` in the
    /// durable catalog, returning the safe id it is addressable by. `None` when this
    /// host has no durable skill store wired (nothing to persist into).
    pub async fn skill_store_put(&self, id: &str, content: &str) -> Option<String> {
        let store = self.skill_store.as_ref()?;
        let out = store
            .put(HOST_SKILL_WORKSPACE, id, content)
            .await
            .expect("persist durable skill");
        // Keep the sync-read cache current for advertisement + scan.
        self.reload_skill_cache().await;
        Some(out)
    }

    /// The ids currently in the durable skill catalog (read straight from the store,
    /// so the CRUD `list` reflects any peer node's writes). Empty when no store is
    /// wired.
    pub async fn skill_store_list(&self) -> Vec<String> {
        match self.skill_store.as_ref() {
            Some(store) => store
                .list(HOST_SKILL_WORKSPACE)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Refresh the in-memory delivered-catalog snapshot from the async store. Called
    /// on a write and at each session's setup so the sync read paths (advertisement,
    /// run-loop scan) see the current catalog.
    pub async fn reload_skill_cache(&self) {
        if let Some(store) = self.skill_store.as_ref() {
            let snapshot = store.list(HOST_SKILL_WORKSPACE).await.unwrap_or_default();
            *self.skill_cache.lock().expect("skill cache poisoned") = snapshot;
        }
    }

    /// A clone of the cached delivered catalog `(id, content)` — the synchronous read
    /// the host's `SkillSource` bridge scans (the run-loop scan cannot await).
    pub fn skill_cache_snapshot(&self) -> Vec<(String, String)> {
        self.skill_cache
            .lock()
            .expect("skill cache poisoned")
            .clone()
    }

    /// Whether this host has a durable skill catalog wired.
    pub fn has_skill_store(&self) -> bool {
        self.skill_store.is_some()
    }

    /// Harvest a thread's read-write memory mounts back into their stores (ADR-0038):
    /// read each realized `.mnt/<logical>` file and persist it under the store id, so a
    /// memory write in this session is visible to the next one that mounts the same id.
    /// The reverse channel behind memory persistence; a no-op for a thread with no
    /// memory mounts or no live environment.
    pub async fn harvest_thread_memory(&self, thread: &str) {
        let (env, mounts) = {
            let sessions = self.sessions.lock().await;
            let env = sessions.get(thread).map(|ctx| ctx.env.clone());
            let mounts = self
                .thread_resources
                .lock()
                .unwrap()
                .get(thread)
                .map(|s| s.memory_mounts.clone())
                .unwrap_or_default();
            (env, mounts)
        };
        let Some(env) = env else {
            return;
        };
        if mounts.is_empty() {
            return;
        }
        // Realized memory mounts live under `.mnt/<logical>`; `list_files(".mnt")` keys
        // each by its path relative to `.mnt/`, i.e. exactly the mount's logical path.
        let realized = env.list_files(".mnt");
        for (store_id, logical) in mounts {
            if let Some((_, bytes)) = realized.iter().find(|(path, _)| *path == logical) {
                self.memory_stores
                    .put(HOST_MEMORY_WORKSPACE, &store_id, bytes)
                    .await
                    .expect("persist harvested memory write-back");
            }
        }
    }

    /// Clone a thread's staged github_repository resources into its freshly-created
    /// environment (ADR-0038). Runs after `provider.create`, host-side, so the token
    /// authenticates the clone transport without ever entering the jail. Fail-closed:
    /// a clone error surfaces so the session doesn't run believing a repo mounted.
    pub(crate) fn provision_thread_repos(
        &self,
        thread: &str,
        env: &awaken_sandbox_local::Environment,
    ) -> Result<(), crate::host::HostError> {
        let repos = self
            .thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|s| s.repos.clone())
            .unwrap_or_default();
        for repo in repos {
            env.provision_repo(
                &repo.logical,
                &repo.url,
                repo.git_ref.as_deref(),
                repo.token.as_ref().map(|t| t.expose_secret()),
            )
            .map_err(|e| crate::host::HostError::internal(e.to_string()))?;
        }
        Ok(())
    }

    /// Push a thread's github_repository edits back to their remotes (ADR-0038
    /// write-back, symmetric to `harvest_thread_memory`): host-side `add`/`commit`/
    /// `push` with the held token. A no-op for a thread with no repos or no live env;
    /// a clean working tree pushes nothing. Best-effort — a push failure is logged by
    /// the caller's context, not fatal to an already-finished turn.
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
        for repo in repos {
            let _ = env.commit_and_push(
                &repo.logical,
                repo.token.as_ref().map(|t| t.expose_secret()),
                "Awaken agent session changes",
            );
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

    /// The skill ids offered on every thread (advertised as the agent's `skills`):
    /// the static configured set plus any durable `/v1/skills` catalog, de-duplicated
    /// with the static set winning, so the advertisement matches what `list_skills`
    /// resolves.
    pub fn skill_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.skills.iter().map(|s| s.id.clone()).collect();
        // The durable catalog is read from the sync cache (refreshed on write and at
        // session setup); a network-DB store cannot be awaited from this sync path.
        for (id, _) in self.skill_cache_snapshot() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// The delegate agent ids (advertised as the agent's `multiagent` roster).
    pub fn delegate_ids(&self) -> Vec<String> {
        self.delegates.iter().cloned().collect()
    }
}

#[cfg(test)]
mod memory_store_tests {
    use super::*;
    use crate::host::SharedHost;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};

    struct NoLlm;
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            unreachable!("the memory-store map never calls the model")
        }
    }

    #[tokio::test]
    async fn create_mints_unique_ids_readable_via_get() {
        let host = SharedHost::new(Arc::new(NoLlm), "test");
        let a = host.create_memory_store().await;
        let b = host.create_memory_store().await;
        assert_ne!(a, b, "each memory store gets a distinct id");
        // A freshly created store exists and is empty; an unknown id is absent — the
        // distinction `prepare_session` relies on to reject a dangling memory binding.
        assert_eq!(host.memory_get(&a).await, Some(Vec::new()));
        assert_eq!(host.memory_get(&b).await, Some(Vec::new()));
        assert_eq!(host.memory_get("memstore_does_not_exist").await, None);
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
    use awaken_sandbox_local::{
        LocalSandboxProvider, ResourceMount, SandboxProvider, SandboxSpec as LocalSpec,
    };

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
    fn resource_mount(logical: &str) -> Mount {
        Mount::Resource(ResourceMount {
            id: format!("id-{logical}"),
            content_hash: String::new(),
            logical_path: logical.to_string(),
            content: format!("content of {logical}"),
        })
    }

    fn repo_stage(logical: &str) -> RepoStage {
        RepoStage {
            logical: logical.to_string(),
            url: "https://example.invalid/x.git".to_string(),
            git_ref: None,
            token: None,
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
                memory_mounts: vec![("s1".into(), "mem-a".into())],
                repos: vec![repo_stage("repo-a")],
            },
        );
        // A second register REPLACES (correct at create time, before any first turn).
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("c.md")],
                prompts: vec!["second".into()],
                memory_mounts: Vec::new(),
                repos: Vec::new(),
            },
        );

        assert_eq!(host.sandbox_spec("t").mounts.len(), 1, "old mounts dropped");
        assert_eq!(
            host.thread_resource_prompts("t"),
            vec!["second".to_string()]
        );
        assert!(host.thread_repos("t").is_empty(), "old repo dropped");
        assert!(host.thread_memory_mounts("t").is_empty());
    }

    #[test]
    fn merge_accumulates_onto_the_staged_set() {
        let host = host();
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("a.md")],
                prompts: vec!["P1".into()],
                memory_mounts: vec![("s1".into(), "mem-a".into())],
                repos: vec![repo_stage("repo-a")],
            },
        );
        // The live `resources.add` path preserves what was already staged.
        host.merge_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("b.md")],
                prompts: vec!["P2".into()],
                memory_mounts: vec![("s2".into(), "mem-b".into())],
                repos: vec![repo_stage("repo-b")],
            },
        );

        assert_eq!(host.sandbox_spec("t").mounts.len(), 2, "both mounts kept");
        assert_eq!(host.thread_resource_prompts("t"), vec!["P1", "P2"]);
        assert_eq!(host.thread_repos("t").len(), 2);
        assert_eq!(host.thread_memory_mounts("t").len(), 2);
    }

    #[test]
    fn remove_drops_only_the_named_logical_across_every_vector() {
        let host = host();
        host.register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("keep.md"), resource_mount("drop.md")],
                prompts: vec!["keep-prompt".into(), "drop-prompt".into()],
                memory_mounts: vec![
                    ("s-keep".into(), "keep.md".into()),
                    ("s-drop".into(), "drop.md".into()),
                ],
                repos: vec![repo_stage("keep.md"), repo_stage("drop.md")],
            },
        );

        // The live `resources.delete` path: drop by realized logical + exact prompt.
        host.remove_thread_resource("t", "drop.md", "drop-prompt");

        let mount_paths: Vec<String> = host
            .sandbox_spec("t")
            .mounts
            .iter()
            .filter_map(|m| {
                m.get("logical_path")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(mount_paths, vec!["keep.md".to_string()]);
        assert_eq!(host.thread_resource_prompts("t"), vec!["keep-prompt"]);
        assert_eq!(
            host.thread_repos("t")
                .iter()
                .map(|r| r.logical.clone())
                .collect::<Vec<_>>(),
            vec!["keep.md".to_string()]
        );
        assert_eq!(
            host.thread_memory_mounts("t"),
            vec![("s-keep".to_string(), "keep.md".to_string())]
        );
    }

    #[test]
    fn sandbox_spec_carries_deny_egress_and_the_staged_mounts() {
        let host = host();
        // No registration and no egress: shared network, no mounts.
        let bare = host.sandbox_spec("t");
        assert!(!bare.deny_egress);
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
        assert!(
            spec.deny_egress,
            "the thread's deny-egress policy is carried"
        );
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(
            spec.mounts[0].get("logical_path").and_then(|v| v.as_str()),
            Some("notes.md")
        );
    }

    #[tokio::test]
    async fn provision_thread_repos_fails_closed_on_an_unsafe_repo_path() {
        // A jail-escaping logical path is rejected by `Environment::provision_repo`
        // BEFORE any git runs (deterministic, no git binary needed). The fail-closed
        // contract: that SandboxError surfaces as a HostError so a session never
        // starts believing a repo mounted when it did not.
        let tmp = tempfile::tempdir().unwrap();
        let env = LocalSandboxProvider::new(tmp.path())
            .create(&LocalSpec::new("s"))
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
                    token: None,
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
        // Stage a memory mount + a repo, but never create a session for the thread:
        // harvest/artifacts must early-return (no live env), not panic.
        host.register_thread_resources(
            "t",
            StagedResources {
                memory_mounts: vec![("s1".into(), "mem.md".into())],
                repos: vec![repo_stage("r")],
                ..Default::default()
            },
        );
        host.harvest_thread_memory("t").await; // no env → no store write
        host.harvest_thread_repo("t").await; // no env → no push
        assert!(host.session_artifacts("t").await.is_empty());
        assert!(host.session_artifacts("never-seen").await.is_empty());
    }
}
