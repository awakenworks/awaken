//! Warm container pool (ADR-0056 §warm-pool): pre-provisioned reusable capacity in
//! front of the container tier, addressing BOTH cold-start latency and resource
//! reuse. A pool keeps up to `size` **fresh** process-as-container agents ready per
//! container shape; a matching session is handed a warm one (skipping create +
//! agent boot on the request path) and the pool replenishes off-path, so the
//! provisioned capacity is continuously reused across sessions.
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
    AgentContainerProvider, AgentContainerSession, ContainerProvider, ContainerRuntime,
    ContainerSandbox, command_of,
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
/// key excludes `scope` (the per-session container name) and includes everything the
/// container is realized from: the agent command, network policy, resource limits,
/// base env, and outputs path. A pure-brain agent (no mounts) of the same shape is
/// reusable; anything with a mount is not.
#[must_use]
pub fn pool_key(spec: &pc::SandboxSpec) -> Option<String> {
    if !spec.mounts.is_empty() {
        return None;
    }
    Some(format!(
        "{:?}|{:?}|{:?}|{:?}|{}",
        command_of(spec),
        spec.network,
        spec.limits,
        spec.env,
        spec.outputs_path
    ))
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
        // Not poolable (declares mounts): create fresh, never pool.
        let Some(key) = pool_key(spec) else {
            return self.inner.open_agent(spec).await;
        };
        // Take a warm container if one is ready, and record this shape's template so a
        // cold miss still teaches the pool what to replenish.
        let warm = {
            let mut map = self.warm.lock().expect("warm pool mutex");
            let entry = map.entry(key.clone()).or_insert_with(|| WarmEntry {
                template: spec.clone(),
                ready: Vec::new(),
            });
            entry.ready.pop()
        };
        let session = match warm {
            // Hit: attach the ACP channel to an already-warmed container (cold-start
            // paid off-path). Its handle carries the warm container's real id.
            Some(sandbox) => self.inner.open_agent_from(sandbox).await?,
            // Miss: create fresh on the request path.
            None => self.inner.open_agent(spec).await?,
        };
        self.spawn_replenish(key);
        Ok(session)
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
        // Different command.
        assert_ne!(pool_key(&base), pool_key(&spec("t", &["other"])));
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
}
