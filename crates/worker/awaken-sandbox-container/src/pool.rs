//! Warm container pool (ADR-0056 §warm-pool): pre-provisioned reusable capacity in
//! front of the container tier, addressing BOTH cold-start latency and resource
//! reuse. A pool keeps up to `size` **empty live environments** ready per container
//! shape; a matching Session binds one and execs its own Native/ACP processes.
//!
//! Safety: only **mount-less** specs are pooled ([`pool_key`] returns `None`
//! otherwise) — a per-session mount bakes session-specific bytes into the container
//! at create, so a pre-warmed container could not serve a different session. Each
//! physical warm container is fresh (never ran a prior session) and serves exactly
//! one session, so there is no cross-session/tenant contamination — the *pool* is
//! reused, not a used container. Reusing a container after a session would require a
//! workspace reset and is deliberately out of scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::{
    AgentContainerProvider, AgentContainerSession, ContainerEnvironment,
    ContainerEnvironmentProvider, ContainerProvider, ContainerRuntime, ContainerSandbox,
    EnvironmentOwnedProcess, RuntimeAgentProcess, command_of,
};

/// A process-global sequence for warm container scopes, so names are unique across
/// every pool in this process (a per-pool counter would collide when two pools warm
/// the same shape). Paired with the pid, warm names are also unique across processes,
/// so a fresh run never collides with a prior run's leaked warm container.
static WARM_SEQ: AtomicU64 = AtomicU64::new(0);

/// A globally-unique scope (container name) for a warm container.
fn next_warm_scope() -> String {
    format!(
        "warmpool-{}-{}",
        std::process::id(),
        WARM_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The container shape two sessions must share for a warm container to be
/// substitutable, or `None` when the spec is not poolable (it declares mounts). The
/// key excludes only `scope` (the per-session logical id) and `extra.command` (the
/// attempt process, spawned after the environment exists). Every other current and
/// future field participates through the serialized normalized spec — including the
/// rootfs/image declaration in `extra.environment`, provider-specific `extra` fields,
/// isolation, network, limits, base env, outputs, and lease policy. This deliberately
/// defaults new fields to **not reusable** until two requests are exactly equivalent,
/// instead of maintaining an allowlist that can silently miss a creation-time field.
/// A mount-less environment of the same shape is reusable; anything with a mount is
/// not.
#[must_use]
pub fn pool_key(spec: &pc::SandboxSpec) -> Option<String> {
    if !spec.mounts.is_empty() {
        return None;
    }
    let mut normalized = spec.clone();
    normalized.scope.clear();
    if let Some(serde_json::Value::Object(extra)) = normalized.extra.as_mut() {
        extra.remove("command");
        if extra.is_empty() {
            normalized.extra = None;
        }
    }
    serde_json::to_string(&normalized).ok()
}

/// One shape's warm capacity: the template spec to replenish from and the ready set.
struct WarmEntry<R: ContainerRuntime> {
    template: pc::SandboxSpec,
    ready: Vec<ContainerSandbox<R>>,
}

/// A warm pool wrapping a concrete [`ContainerProvider`]. Implements
/// [`AgentContainerProvider`], so it drops into the host's container channel source
/// exactly where a bare provider would (`build_docker_source`/`build_k8s_source`).
pub struct WarmContainerPool<R: ContainerRuntime> {
    inner: Arc<ContainerProvider<R>>,
    size: usize,
    warm: Arc<Mutex<HashMap<String, WarmEntry<R>>>>,
    /// Set by [`shutdown`](Self::shutdown) so in-flight replenishment stops adding
    /// containers after the drain — otherwise a replenish that lands mid-drain would
    /// leak a warm container past teardown.
    closed: Arc<AtomicBool>,
}

impl<R: ContainerRuntime + 'static> WarmContainerPool<R> {
    /// Wrap `inner`, keeping up to `size` warm containers ready per shape. `size == 0`
    /// is a pass-through (every `open_agent` creates fresh) — the safe default a
    /// deployment opts out of the warm cost with.
    #[must_use]
    pub fn new(inner: Arc<ContainerProvider<R>>, size: usize) -> Self {
        Self {
            inner,
            size,
            warm: Arc::new(Mutex::new(HashMap::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A unique per-warm container scope, so pre-warmed names never collide with a
    /// session's thread-scoped name or each other.
    fn warm_scope(&self) -> String {
        next_warm_scope()
    }

    /// Eagerly create `n` warm containers for `spec`'s shape (a no-op for a
    /// non-poolable spec). Deterministic (awaited), for startup pre-warm and tests.
    pub async fn prewarm(&self, spec: &pc::SandboxSpec, n: usize) -> Result<(), pc::SandboxError> {
        let Some(key) = pool_key(spec) else {
            return Ok(());
        };
        for _ in 0..n {
            let mut warm_spec = spec.clone();
            warm_spec.scope = self.warm_scope();
            let sandbox = self.inner.create_container(&warm_spec).await?;
            let mut map = self.warm.lock().expect("warm pool mutex");
            map.entry(key.clone())
                .or_insert_with(|| WarmEntry {
                    template: spec.clone(),
                    ready: Vec::new(),
                })
                .ready
                .push(sandbox);
        }
        Ok(())
    }

    /// How many warm containers are ready for `spec`'s shape (0 when not poolable).
    #[must_use]
    pub fn ready_len(&self, spec: &pc::SandboxSpec) -> usize {
        pool_key(spec)
            .and_then(|k| {
                self.warm
                    .lock()
                    .expect("warm pool mutex")
                    .get(&k)
                    .map(|e| e.ready.len())
            })
            .unwrap_or(0)
    }

    /// Dispose every warm container (a graceful drain). Stops replenishment first, then
    /// drains repeatedly so a create that was already in flight when the drain started
    /// is disposed too rather than leaked. Best-effort per container.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        loop {
            let batch: Vec<ContainerSandbox<R>> = {
                let mut map = self.warm.lock().expect("warm pool mutex");
                map.drain().flat_map(|(_, e)| e.ready).collect()
            };
            if batch.is_empty() {
                break;
            }
            for sandbox in batch {
                let _ = pc::Sandbox::dispose(&sandbox).await;
            }
            // Yield so any in-flight replenish create lands, then drain it too.
            tokio::task::yield_now().await;
        }
    }

    /// Replenish `key` back to `size` off the request path (bounded). Concurrent
    /// replenishers for the same key may briefly overshoot; the cap check discards
    /// (and disposes) the excess, so the steady state is exactly `size`.
    fn spawn_replenish(&self, key: String) {
        if self.size == 0 || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        let warm = self.warm.clone();
        let closed = self.closed.clone();
        let size = self.size;
        tokio::spawn(async move {
            loop {
                if closed.load(Ordering::SeqCst) {
                    return;
                }
                // How many are missing + the shape to build, read under the lock.
                let template = {
                    let map = warm.lock().expect("warm pool mutex");
                    match map.get(&key) {
                        Some(e) if e.ready.len() < size => e.template.clone(),
                        _ => return,
                    }
                };
                let mut warm_spec = template;
                warm_spec.scope = next_warm_scope();
                match inner.create_container(&warm_spec).await {
                    Ok(sandbox) => {
                        // Re-check the cap under the lock; dispose an over-cap create.
                        let over = {
                            let mut map = warm.lock().expect("warm pool mutex");
                            match map.get_mut(&key) {
                                Some(e) if e.ready.len() < size => {
                                    e.ready.push(sandbox);
                                    None
                                }
                                _ => Some(sandbox),
                            }
                        };
                        if let Some(excess) = over {
                            let _ = pc::Sandbox::dispose(&excess).await;
                            return;
                        }
                    }
                    // A create failure stops replenishing this key (fail closed, no spin).
                    Err(_) => return,
                }
            }
        });
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> AgentContainerProvider for WarmContainerPool<R> {
    async fn open_agent(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<AgentContainerSession, pc::SandboxError> {
        let environment = self.create_environment(spec).await?;
        let argv = command_of(spec);
        if argv.is_empty() {
            return Err(pc::SandboxError::new("agent command argv is empty"));
        }
        let RuntimeAgentProcess { process, channel } = environment
            .spawn_agent_process(pc::Command {
                argv,
                cwd: String::new(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await?;
        Ok(AgentContainerSession {
            channel,
            process: Box::new(EnvironmentOwnedProcess {
                inner: process,
                environment: environment.clone(),
            }),
            handle: environment.handle(),
        })
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> ContainerEnvironmentProvider for WarmContainerPool<R> {
    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.inner.install_memory_mounter(mounter);
    }

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        let Some(key) = pool_key(spec) else {
            return self.inner.create_environment(spec).await;
        };
        let warm = {
            let mut map = self.warm.lock().expect("warm pool mutex");
            let entry = map.entry(key.clone()).or_insert_with(|| WarmEntry {
                template: spec.clone(),
                ready: Vec::new(),
            });
            entry.ready.pop()
        };
        let environment = match warm {
            Some(environment) => environment.bind_scope(spec.scope.clone()),
            None => self.inner.create_container(spec).await?,
        };
        self.spawn_replenish(key);
        Ok(Arc::new(environment))
    }

    async fn adopt_environment(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        self.inner.adopt_environment(handle).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(scope: &str, cmd: &[&str]) -> pc::SandboxSpec {
        pc::SandboxSpec {
            scope: scope.into(),
            isolation: pc::IsolationClass::Container,
            mounts: Vec::new(),
            env: Vec::new(),
            network: pc::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: Some(serde_json::json!({ "command": cmd })),
        }
    }

    #[test]
    fn a_mount_less_spec_is_poolable_and_keys_on_shape_not_scope() {
        // Two sessions (different scope) of the same mount-less agent share a key —
        // so a warm container for one can serve the other.
        let a = spec("thread-a", &["agent", "--acp"]);
        let b = spec("thread-b", &["agent", "--acp"]);
        assert_eq!(pool_key(&a), pool_key(&b));
        assert!(pool_key(&a).is_some());
    }

    #[test]
    fn a_different_shape_gets_a_different_key() {
        let base = spec("t", &["agent", "--acp"]);
        // Attempt commands do not change the empty environment shape.
        assert_eq!(pool_key(&base), pool_key(&spec("t", &["other"])));
        // Different network.
        let mut net = spec("t", &["agent", "--acp"]);
        net.network = pc::NetworkPolicy::Unrestricted;
        assert_ne!(pool_key(&base), pool_key(&net));
        // Different limits.
        let mut lim = spec("t", &["agent", "--acp"]);
        lim.limits = pc::ResourceLimits {
            memory_bytes: Some(1 << 20),
            ..Default::default()
        };
        assert_ne!(pool_key(&base), pool_key(&lim));

        // Different base environment.
        let mut env = base.clone();
        env.env.push(pc::EnvVar {
            name: "MODE".into(),
            value: pc::EnvValue::Inline {
                value: "strict".into(),
            },
            visibility: pc::EnvVisibility::Process,
        });
        assert_ne!(pool_key(&base), pool_key(&env));

        // Different artifact root, isolation requirement, or lease policy.
        let mut outputs = base.clone();
        outputs.outputs_path = "/different/outputs".into();
        assert_ne!(pool_key(&base), pool_key(&outputs));
        let mut isolation = base.clone();
        isolation.isolation = pc::IsolationClass::Namespace;
        assert_ne!(pool_key(&base), pool_key(&isolation));
        let mut lease = base.clone();
        lease.lease_ttl_secs = Some(60);
        assert_ne!(pool_key(&base), pool_key(&lease));
    }

    #[test]
    fn rootfs_and_every_provider_extra_field_partition_warm_capacity() {
        let base = spec("thread-a", &["agent", "--acp"]);

        let mut image = spec("thread-b", &["different-attempt"]);
        image.extra = Some(serde_json::json!({
            "command": ["different-attempt"],
            "environment": { "kind": "image", "reference": "agent:v2" }
        }));
        assert_ne!(pool_key(&base), pool_key(&image));

        let mut other_image = image.clone();
        other_image.extra.as_mut().unwrap()["environment"]["reference"] =
            serde_json::json!("agent:v3");
        assert_ne!(pool_key(&image), pool_key(&other_image));

        let mut private_root = image.clone();
        private_root.extra = Some(serde_json::json!({
            "environment": {
                "kind": "isolated_root",
                "base": { "source": "dir", "path_template": "/roots/agent" },
                "writable_base": false
            }
        }));
        assert_ne!(pool_key(&image), pool_key(&private_root));

        let mut seccomp = image.clone();
        seccomp.extra.as_mut().unwrap()["seccomp_profile"] = serde_json::json!("strict-v1");
        assert_ne!(pool_key(&image), pool_key(&seccomp));
    }

    #[test]
    fn scope_and_attempt_command_are_the_only_ignored_fields() {
        let mut a = spec("thread-a", &["agent", "--acp"]);
        a.extra.as_mut().unwrap()["environment"] =
            serde_json::json!({ "kind": "image", "reference": "agent:v2" });
        let mut b = a.clone();
        b.scope = "thread-b".into();
        b.extra.as_mut().unwrap()["command"] = serde_json::json!(["other", "attempt"]);
        assert_eq!(pool_key(&a), pool_key(&b));
    }

    #[test]
    fn a_spec_with_a_mount_is_not_poolable() {
        // A per-session mount bakes session bytes at create — never pool it.
        let mut m = spec("t", &["agent", "--acp"]);
        m.mounts = vec![pc::MountRequirement {
            mount_id: "m".into(),
            source: pc::MountSource::Inline {
                contents: "x".into(),
            },
            mount_path: "/x".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }];
        assert_eq!(pool_key(&m), None);
    }

    #[test]
    fn pool_key_is_none_for_every_mount_source_kind() {
        // No cross-session contamination: ANY declared mount — regardless of source kind
        // — makes a spec non-poolable, because every kind either bakes session-specific
        // bytes (File/Resource/Secret/Inline/Other) or binds a session-specific store /
        // host path (MemoryStore/CacheVolume) into the container at create. A warm
        // container pre-built without those could never serve a different session.
        let sources = [
            pc::MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            pc::MountSource::Resource {
                resource_id: "r".into(),
                content_hash: None,
            },
            pc::MountSource::Secret {
                reference: "broker://k".into(),
                content_hash: None,
            },
            pc::MountSource::MemoryStore {
                store_id: "s".into(),
            },
            pc::MountSource::Inline {
                contents: "x".into(),
            },
            pc::MountSource::CacheVolume {
                host_path: "/cache".into(),
                key: "k".into(),
            },
            pc::MountSource::Other(serde_json::json!({ "content": "x" })),
        ];
        for source in sources {
            let mut m = spec("t", &["agent", "--acp"]);
            let label = format!("{source:?}");
            m.mounts = vec![pc::MountRequirement {
                mount_id: "m".into(),
                source,
                mount_path: "/x".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            }];
            assert_eq!(pool_key(&m), None, "a {label} mount must not be poolable");
        }
    }
}
