//! Session Environment adoption, fencing, and terminal disposal.

use super::*;

impl SharedHost {
    pub(crate) async fn session_environment(
        &self,
        thread: &str,
    ) -> Option<Arc<crate::session_environment::SessionEnvironment>> {
        self.session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten()
    }

    #[cfg(test)]
    pub(crate) async fn session_environment_handle(
        &self,
        thread: &str,
    ) -> Option<awaken_provisioning_contract::SandboxHandle> {
        self.session_environment(thread)
            .await
            .map(|env| env.handle())
    }

    /// Resolve an opaque durable binding through the one Session environment
    /// provider. Both claimed-worker recovery and foreground Managed-session
    /// restoration use this path, so ownership/status validation cannot drift.
    /// `rebuild_unavailable` is the dispatch recovery policy: when set, an
    /// unavailable binding is fenced and reported to the caller for replacement.
    pub(crate) async fn adopt_bound_session_environment(
        &self,
        thread: &str,
        encoded: Option<&str>,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
        rebuild_unavailable: bool,
    ) -> Result<(Option<crate::session_environment::SessionEnvironment>, bool), HostError> {
        let Some(encoded) = encoded else {
            return Ok((None, false));
        };
        let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(encoded)
            .map_err(|error| {
                HostError::internal(format!("invalid Session sandbox binding: {error}"))
            })?;
        if handle.sandbox_id != thread {
            return Err(HostError::internal(format!(
                "sandbox {} does not belong to Session {thread}",
                handle.sandbox_id
            )));
        }
        if let Some(environment) = self.session_environment(thread).await {
            let resident = environment.handle();
            if resident != handle {
                return Err(HostError::internal(format!(
                    "Session {thread} is already bound to sandbox {}, not {}",
                    resident.sandbox_id, handle.sandbox_id
                )));
            }
            match environment.status().await {
                Ok(awaken_provisioning_contract::SandboxStatus::Ready) => {
                    return Ok((None, false));
                }
                Ok(_) | Err(_) if rebuild_unavailable => {
                    if !self.discard_session_environment(thread, &environment).await {
                        return Err(HostError::internal(format!(
                            "lost the sandbox recovery fence for Session {thread}"
                        )));
                    }
                    return Ok((None, true));
                }
                Ok(status) => {
                    return Err(HostError::internal(format!(
                        "Session sandbox {} is not ready ({status:?})",
                        handle.sandbox_id
                    )));
                }
                Err(error) => {
                    return Err(HostError::internal(format!(
                        "could not inspect Session sandbox {}: {error}",
                        handle.sandbox_id
                    )));
                }
            }
        }
        let provider = self.session_environment_provider(provisioning)?;
        let adoption = async {
            let sandbox = provider
                .adopt(&handle)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            if sandbox
                .status()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
                != awaken_provisioning_contract::SandboxStatus::Ready
            {
                return Err(HostError::internal(format!(
                    "Session sandbox {} is no longer available",
                    handle.sandbox_id
                )));
            }
            sandbox
                .reconcile_adopted_mounts(&self.thread_session_mounts(thread))
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            Ok(sandbox)
        }
        .await;
        match adoption {
            Ok(sandbox) => Ok((Some(sandbox), false)),
            Err(_) if rebuild_unavailable => Ok((None, true)),
            Err(error) => Err(error),
        }
    }

    /// Forget a dead environment only when it is still the exact `Arc` observed
    /// by the recovery attempt. Object identity plus the full durable handle is
    /// the ABA fence: a stale recovery task must never evict a replacement that
    /// has already been installed for the same thread/sandbox id.
    pub(crate) async fn discard_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        let removed = self
            .session_slots
            .modify(thread, |slot| {
                let still_observed = slot.environment.as_ref().is_some_and(|current| {
                    Arc::ptr_eq(current, expected) && current.handle() == expected.handle()
                });
                if !still_observed {
                    return None;
                }
                let removed = slot.environment.take();
                // Both eager contexts (which retain `env`) and deferred contexts
                // (which retain only the lazy executor) are coupled to this exact
                // slot environment once it is published.
                slot.runtime = None;
                removed
            })
            .flatten();
        if let Some(environment) = removed {
            // The provider object may represent an unavailable external
            // sandbox. Only stop processes owned by this wrapper here; normal
            // terminal disposal remains the responsibility of `end_session`.
            environment.stop_bound_processes().await;
            true
        } else {
            false
        }
    }

    /// End a session's sandbox lifecycle at a terminal edge (managed session
    /// delete/archive): evict the cached context and dispose the sandbox at the OS
    /// boundary (shred materialized secrets, reap the per-thread workspace dir).
    /// Idempotent — a thread with no live session is a no-op.
    ///
    /// This is the ONLY place a Session-owned environment is reaped. Runtime-context
    /// rebuilds retain it in the Session runtime slot; a terminal end removes that
    /// owner and disposes exactly once. Repository publication and authored-Skill
    /// persistence run at the caller's release boundary before this method; Memory
    /// copy reconciliation is owned by `Sandbox::dispose` through its mount guard.
    pub(crate) async fn end_session(&self, thread: &str) -> Result<(), HostError> {
        self.stop_session_mcp_processes(thread).await;
        let (ctx, env, expected_binding) = self.session_slots.update(thread, |slot| {
            (
                slot.runtime.take(),
                slot.environment.take(),
                slot.expected_environment_binding.clone(),
            )
        });
        let mut adopted_for_cleanup = false;
        let env = env.or_else(|| ctx.and_then(|ctx| ctx.env.clone()));
        let env = if env.is_some() {
            env
        } else if let Some(binding) = expected_binding.as_deref() {
            // A Runtime context can be evicted or move between all-in-one
            // components while the durable Session still owns its Sandbox. A
            // missing process-local Arc is therefore not evidence that cleanup
            // is complete: adopt the exact fenced handle and reconcile writable
            // Memory before issuing a successful terminal receipt.
            let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(binding)
                .map_err(|error| {
                    HostError::internal(format!(
                        "invalid terminal Session sandbox binding: {error}"
                    ))
                })?;
            if handle.sandbox_id != thread {
                return Err(HostError::internal(format!(
                    "terminal sandbox {} does not belong to Session {thread}",
                    handle.sandbox_id
                )));
            }
            let adopted = self
                .session_provider
                .adopt(&handle)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            adopted_for_cleanup = true;
            Some(Arc::new(adopted))
        } else {
            None
        };
        let dispose_result = if let Some(env) = env {
            if adopted_for_cleanup || env.needs_recovered_memory_reconciliation() {
                let mounter = self.memory_mounter().ok_or_else(|| {
                    HostError::internal("recovered Memory copy has no MemoryMounter")
                })?;
                // Recovered cleanup must reconcile both Resource mounts and
                // Session-baseline mounts (Dream uses the latter). Normal live
                // disposal already harvests every MemoryMount guard; omitting
                // baseline mounts only on recovery would make crash behavior
                // diverge and could lose a completed Dream output.
                for mount in self.thread_session_mounts(thread) {
                    if let awaken_provisioning_contract::MountSource::MemoryStore {
                        store_id,
                        materialization_reference,
                        ..
                    } = &mount.source
                    {
                        let files = env
                            .list_frozen_mount_files(&mount.mount_path)
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                        mounter
                            .reconcile_recovered_copy(
                                materialization_reference.as_deref().unwrap_or(store_id),
                                &files,
                                mount.access,
                            )
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                    }
                }
            }
            env.dispose()
                .await
                .map_err(|e| HostError::internal(e.to_string()))
        } else {
            Ok(())
        };

        // Terminal cleanup removes every thread-scoped projection, including
        // credential-bearing MCP relay routes. A future Session reusing the opaque
        // thread id must start from an empty projection and be authorized/staged
        // again; resource state never outlives its Session boundary in these maps.
        self.session_slots.remove(thread);
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(thread);
        }

        dispose_result
    }
}
