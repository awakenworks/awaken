// @formal entrypoint credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
//! The Session MCP lifecycle over the Runtime Host's Session slots: exact
//! generation owners are staged, published, renewed and drained here, and the
//! Worker relay that serves authenticated generations is owned here. The Host
//! supplies its slot registry through [`McpSessionSlots`] and its root
//! realization authority through [`McpGenerationAuthority`]; this value holds
//! only the narrow dependencies the lifecycle uses.

use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::mcp::McpError;
use crate::mcp_material::{
    McpGenerationProjection, McpProjectionState, McpWiring, SessionMcpState,
};
use crate::mcp_relay::{McpRelay, McpRelayOutboundProxy};

const MCP_GENERATION_CALL_QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(10);

/// The Runtime Host's per-Session slot registry, seen only through each
/// slot's [`SessionMcpState`] and its cached Session Runtime. Every closure
/// runs under the registry's own lock, exactly like a direct slot access.
pub trait McpSessionSlots: Clone + Send + Sync + 'static {
    /// The Session Runtime a slot caches; publication invalidates it and drain
    /// cancels and awaits its active Run.
    type Runtime: Clone + Send + Sync;

    /// Read an existing Session's MCP state.
    fn read_mcp<R>(&self, session: &str, read: impl FnOnce(&SessionMcpState) -> R) -> Option<R>;

    /// Change a Session's MCP state, creating its slot when absent.
    fn update_mcp<R>(&self, session: &str, update: impl FnOnce(&mut SessionMcpState) -> R) -> R;

    /// Change an existing Session's MCP state.
    fn modify_mcp<R>(
        &self,
        session: &str,
        modify: impl FnOnce(&mut SessionMcpState) -> R,
    ) -> Option<R>;

    /// Change an existing Session's MCP state together with its cached Runtime
    /// in one slot transaction.
    fn modify_mcp_and_runtime<R>(
        &self,
        session: &str,
        modify: impl FnOnce(&mut SessionMcpState, &mut Option<Self::Runtime>) -> R,
    ) -> Option<R>;

    /// Cancel the Runtime's current Run, if any.
    fn cancel_runtime(runtime: &Self::Runtime);

    /// Whether the Runtime still has an active Run.
    fn runtime_has_active_run(runtime: &Self::Runtime) -> bool;
}

/// The Runtime Host's root realization authority over one exact MCP
/// generation. Absence is explicit: it is never a live lease or proof of
/// renewal.
pub trait McpGenerationAuthority: Send + Sync {
    fn mcp_generation_root_authority(
        &self,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
    ) -> Option<bool>;
}

/// The Session MCP lifecycle and the Worker relay it owns. Cloning shares the
/// one relay.
#[derive(Clone)]
pub struct McpSessions {
    /// The host's loopback MCP relay (α-reference resolver), started lazily on
    /// the first sandboxed ACP session that stages an authenticated MCP server.
    /// It holds the real bearers host-side and injects them when forwarding the
    /// sandbox's MCP calls, so the raw token never enters the sandbox.
    pub(crate) relay: Arc<tokio::sync::OnceCell<McpRelay>>,
    /// Deployment-owned exposure of the relay to isolated container networks;
    /// `None` keeps the relay on Worker loopback.
    relay_exposure: Option<crate::ContainerMcpRelayExposure>,
    /// Installed only by a deployment that exposes this relay to isolated
    /// containers. It issues existing Gateway CONNECT capabilities per call.
    outbound_proxy: Option<Arc<dyn McpRelayOutboundProxy>>,
    /// One process-wide reaper for the exact MCP process handles tracked in
    /// Session projections. Composition installs the existing Supervisor.
    process_reaper: Option<Arc<dyn awaken_session_environment_contract::ProcessReaper>>,
    /// The process clock relay route admission reads.
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
}

impl McpSessions {
    #[must_use]
    pub fn new(
        relay_exposure: Option<crate::ContainerMcpRelayExposure>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            relay: Arc::new(tokio::sync::OnceCell::new()),
            relay_exposure,
            outbound_proxy: None,
            process_reaper: None,
            clock,
        }
    }

    /// Fixtures replace a constructed Host's deployment, and its relay
    /// exposure with it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_relay_exposure(&mut self, exposure: Option<crate::ContainerMcpRelayExposure>) {
        self.relay_exposure = exposure;
    }

    pub fn set_outbound_proxy(&mut self, issuer: Arc<dyn McpRelayOutboundProxy>) {
        self.outbound_proxy = Some(issuer);
    }

    pub fn set_process_reaper(
        &mut self,
        reaper: Arc<dyn awaken_session_environment_contract::ProcessReaper>,
    ) {
        self.process_reaper = Some(reaper);
    }

    #[must_use]
    pub fn relay_exposure(&self) -> Option<&crate::ContainerMcpRelayExposure> {
        self.relay_exposure.as_ref()
    }

    pub fn process_reaper(
        &self,
    ) -> Result<Arc<dyn awaken_session_environment_contract::ProcessReaper>, McpError> {
        self.process_reaper.clone().ok_or_else(|| {
            McpError::unavailable_classified(
                "mcp_generation_process_reaper_missing",
                "MCP generation process reaper is not installed",
            )
        })
    }

    /// The already-started relay. Runtime construction reads routes staged by
    /// the realization lifecycle; it never starts a relay itself.
    #[must_use]
    pub fn relay(&self) -> Option<&McpRelay> {
        self.relay.get()
    }

    /// Remove residual bearer-bearing routes only after canonical per-
    /// generation drain has succeeded for the whole Session.
    pub fn remove_relay_routes(&self, thread: &str) {
        if let Some(relay) = self.relay.get() {
            relay.remove_routes(thread);
        }
    }

    /// Start a relay bound to this deployment's exposure and outbound proxy,
    /// admitting routes by `authority`'s current root.
    pub async fn start_relay(
        &self,
        authority: Weak<dyn McpGenerationAuthority>,
    ) -> std::io::Result<McpRelay> {
        McpRelay::for_authority(
            authority,
            self.relay_exposure.as_ref(),
            self.clock.clone(),
            self.outbound_proxy.clone(),
        )
        .await
    }

    /// The one process relay, started on first use.
    pub async fn relay_or_start(
        &self,
        authority: Weak<dyn McpGenerationAuthority>,
    ) -> std::io::Result<&McpRelay> {
        self.relay
            .get_or_try_init(|| self.start_relay(authority))
            .await
    }

    /// Bind the configured external relay before a Worker can publish Ready.
    /// The endpoint is still only a transport prerequisite: provider-owned
    /// substitution and no-bypass evidence remain independent admission facts.
    pub async fn prepare_container_relay(
        &self,
        authority: Weak<dyn McpGenerationAuthority>,
    ) -> Result<(), std::io::Error> {
        if self.relay_exposure.is_none() {
            return Ok(());
        }
        self.relay_or_start(authority).await?;
        if !self.relay.get().is_some_and(McpRelay::is_live) {
            return Err(std::io::Error::other(
                "container MCP relay transport exited during startup",
            ));
        }
        Ok(())
    }

    /// Install an already-started relay as the process relay.
    #[cfg(test)]
    pub fn install_relay(&self, relay: McpRelay) -> bool {
        self.relay.set(relay).is_ok()
    }
}

impl McpSessions {
    pub fn active_projections<S: McpSessionSlots>(
        &self,
        slots: &S,
        thread: &str,
    ) -> Vec<McpGenerationProjection> {
        slots
            .read_mcp(thread, |mcp| {
                if mcp.quiescence_fence.is_some() {
                    return Vec::new();
                }
                mcp.projections
                    .iter()
                    .filter(|projection| {
                        projection.state == McpProjectionState::Active
                            && projection.server.is_some()
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn projection<S: McpSessionSlots>(
        &self,
        slots: &S,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
    ) -> Option<McpGenerationProjection> {
        slots
            .read_mcp(generation.session_id().as_str(), |mcp| {
                mcp.projections
                    .iter()
                    .find(|projection| projection.request.generation == *generation)
                    .cloned()
            })
            .flatten()
    }

    pub fn insert_projection<S: McpSessionSlots>(
        &self,
        slots: &S,
        projection: McpGenerationProjection,
    ) -> Result<(), McpError> {
        let thread = projection.request.generation.session_id().clone();
        slots.update_mcp(thread.as_str(), |mcp| {
            if mcp.quiescence_fence.is_some() {
                return Err(McpError::unavailable_classified(
                    "session_environment_quiescing",
                    "MCP staging is closed while the Session Environment quiesces",
                ));
            }
            if mcp
                .projections
                .iter()
                .any(|existing| existing.request.generation == projection.request.generation)
            {
                return Err(McpError::internal(
                    "MCP generation projection already exists with another realization",
                ));
            }
            mcp.projections.push(projection);
            Ok(())
        })
    }

    /// Transfer a freshly spawned stdio process into the already-installed
    /// exact-generation owner before any further fallible await.
    pub fn attach_staging_process<S: McpSessionSlots>(
        &self,
        slots: &S,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
        process: Arc<dyn awaken_provisioning_contract::ProcessHandle>,
    ) -> Result<bool, McpError> {
        slots
            .modify_mcp(generation.session_id().as_str(), |mcp| {
                let admitted = mcp.quiescence_fence.is_none();
                let projection = mcp
                    .projections
                    .iter_mut()
                    .find(|projection| projection.request.generation == *generation)
                    .ok_or_else(|| McpError::internal("unknown MCP staging owner"))?;
                if !matches!(
                    projection.state,
                    McpProjectionState::Staging | McpProjectionState::Draining
                ) || projection.staging.is_none()
                    || projection.mcp_process.is_some()
                {
                    return Err(McpError::internal(
                        "MCP staging owner no longer admits a spawned process",
                    ));
                }
                let connect_admitted = admitted && projection.state == McpProjectionState::Staging;
                projection.mcp_process = Some(process);
                Ok(connect_admitted)
            })
            .unwrap_or_else(|| Err(McpError::internal("unknown MCP Session projection")))
    }

    /// Publish only wiring into the private staged projection. Durable publish
    /// remains the separate `Staged -> Active` transition.
    pub fn complete_staging<S: McpSessionSlots>(
        &self,
        slots: &S,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
        wiring: McpWiring,
    ) -> Result<(), McpError> {
        slots
            .modify_mcp(generation.session_id().as_str(), |mcp| {
                if mcp.quiescence_fence.is_some() {
                    return Err(McpError::unavailable_classified(
                        "session_environment_quiescing",
                        "MCP staging cannot commit while the Session Environment quiesces",
                    ));
                }
                let projection = mcp
                    .projections
                    .iter_mut()
                    .find(|projection| projection.request.generation == *generation)
                    .ok_or_else(|| McpError::internal("unknown MCP staging owner"))?;
                if projection.state != McpProjectionState::Staging
                    || projection.mcp_process.is_none()
                {
                    return Err(McpError::internal(
                        "MCP staging owner changed before connection completed",
                    ));
                }
                projection.native_wiring = Some(wiring);
                projection.staging = None;
                projection.state = McpProjectionState::Staged;
                Ok(())
            })
            .unwrap_or_else(|| Err(McpError::internal("unknown MCP Session projection")))
    }

    /// Extend one already-staged or active exact generation without reopening
    /// credential material or reconnecting MCP. The immutable realization
    /// binding must be identical and only the expiry/idempotency attempt may
    /// advance. Supporting Staged closes the race where Control renews while the
    /// first stage effect is in flight and republishes its longer exact fence.
    pub fn renew_projection<S: McpSessionSlots>(
        &self,
        slots: &S,
        request: &awaken_session_mcp_contract::StageMcpAttachment,
    ) -> Result<Option<awaken_session_mcp_contract::McpRealizationReceipt>, McpError> {
        let binding = request.renewal_binding_fingerprint();
        slots.update_mcp(request.generation.session_id().as_str(), |mcp| {
            if mcp.quiescence_fence.is_some() {
                return Err(McpError::unavailable_classified(
                    "session_environment_quiescing",
                    "MCP renewal is closed while the Session Environment quiesces",
                ));
            }
            let Some(projection) = mcp.projections.iter_mut().find(|projection| {
                projection.request.generation.session_id() == request.generation.session_id()
                    && projection.request.generation.attachment_id()
                        == request.generation.attachment_id()
                    && projection.request.generation.generation() == request.generation.generation()
                    && projection.request.generation.runtime_incarnation()
                        == request.generation.runtime_incarnation()
                    && projection.request.generation.lease_epoch()
                        == request.generation.lease_epoch()
            }) else {
                return Ok(None);
            };
            if !matches!(
                projection.state,
                McpProjectionState::Staged | McpProjectionState::Active
            ) || projection.request.realization_id != request.realization_id
                || projection.request.renewal_binding_fingerprint() != binding
                || request.generation.lease_expires_at_unix_ms()
                    <= projection.request.generation.lease_expires_at_unix_ms()
            {
                return Err(McpError::internal(
                    "MCP lease renewal conflicts with the staged realization",
                ));
            }
            let receipt = awaken_session_mcp_contract::McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                actual_realization_kind: projection.receipt.actual_realization_kind,
                receipt_fingerprint: request.fingerprint(),
            };
            projection.request = request.clone();
            projection.receipt = receipt.clone();
            Ok(Some(receipt))
        })
    }

    /// Forget an already-cleaned process-local tombstone when Control replays
    /// the exact Stage or renews only its lease/attempt key. A removed projection
    /// owns no remaining process, route, or credential material, so retaining
    /// it must not permanently prevent the canonical realization driver from
    /// recovering after a concurrent Control CAS and heartbeat. Changed immutable
    /// bindings and non-monotonic renewals remain resident. The caller has already
    /// checked current ownership; removing a tombstone grants no staged receipt.
    pub fn forget_replayable_removed_projection<S: McpSessionSlots>(
        &self,
        slots: &S,
        request: &awaken_session_mcp_contract::StageMcpAttachment,
    ) -> bool {
        let request_fingerprint = request.fingerprint();
        let renewal_binding = request.renewal_binding_fingerprint();
        slots
            .modify_mcp(request.generation.session_id().as_str(), |mcp| {
                if mcp.quiescence_fence.is_some() {
                    return false;
                }
                let Some(index) = mcp.projections.iter().position(|projection| {
                    projection.state == McpProjectionState::Removed
                        && ((projection.request == *request
                            && projection.receipt.receipt_fingerprint == request_fingerprint)
                            || (projection.request.renewal_binding_fingerprint()
                                == renewal_binding
                                && request.generation.lease_expires_at_unix_ms()
                                    > projection.request.generation.lease_expires_at_unix_ms()))
                }) else {
                    return false;
                };
                mcp.projections.remove(index);
                true
            })
            .unwrap_or(false)
    }

    pub async fn publish_projection<S: McpSessionSlots>(
        &self,
        slots: &S,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), McpError> {
        if !slots
            .read_mcp(generation.session_id().as_str(), |mcp| {
                mcp.admits_realization()
            })
            .unwrap_or(true)
        {
            return Err(McpError::unavailable_classified(
                "session_environment_quiescing",
                "MCP publication is closed while the Session Environment quiesces",
            ));
        }
        if let Some(projection) = self.projection(slots, generation) {
            if matches!(
                projection.state,
                McpProjectionState::Staging
                    | McpProjectionState::Draining
                    | McpProjectionState::Removed
            ) {
                return Err(McpError::internal(
                    "non-visible MCP generation cannot be published",
                ));
            }
            if let Some(server) = &projection.server
                && server.bearer().is_some()
                && projection.native_wiring.is_none()
            {
                match projection.receipt.actual_realization_kind {
                    // `None` is the pre-receipt-discriminator legacy form. It
                    // remains safe only through the same exact staged relay;
                    // it can never fall through to process injection.
                    Some(awaken_runtime_contract::CredentialRealizationKind::WorkerRelay)
                    | None => {
                        let relay = self.relay.get().ok_or_else(|| {
                            McpError::internal("authenticated MCP generation has no staged relay")
                        })?;
                        if !relay.update_staged_route(generation, server) {
                            return Err(McpError::internal(
                                "authenticated MCP generation has no exact staged relay route",
                            ));
                        }
                    }
                    Some(
                        awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField,
                    ) => {}
                    _ => {
                        return Err(McpError::internal(
                            "authenticated MCP generation has no admitted delivery",
                        ));
                    }
                }
            }
        }
        let changed =
            slots.modify_mcp_and_runtime(generation.session_id().as_str(), |mcp, runtime| {
                if mcp.quiescence_fence.is_some() {
                    return Err(McpError::unavailable_classified(
                        "session_environment_quiescing",
                        "MCP publication is closed while the Session Environment quiesces",
                    ));
                }
                let Some(index) = mcp
                    .projections
                    .iter()
                    .position(|projection| projection.request.generation == *generation)
                else {
                    return Err(McpError::internal("unknown MCP generation projection"));
                };
                match mcp.projections[index].state {
                    McpProjectionState::Staging => Err(McpError::internal(
                        "MCP generation is still staging and cannot be published",
                    )),
                    McpProjectionState::Staged => {
                        for projection in &mut mcp.projections {
                            if projection.request.generation.attachment_id()
                                == generation.attachment_id()
                                && projection.state == McpProjectionState::Active
                            {
                                projection.state = McpProjectionState::Draining;
                            }
                        }
                        mcp.projections[index].state = McpProjectionState::Active;
                        *runtime = None;
                        Ok(true)
                    }
                    McpProjectionState::Active => Ok(false),
                    McpProjectionState::Draining | McpProjectionState::Removed => Err(
                        McpError::internal("non-visible MCP generation cannot be published"),
                    ),
                }
            });
        match changed {
            Some(result) => {
                result?;
                Ok(())
            }
            None => Err(McpError::internal("unknown MCP Session projection")),
        }
    }

    pub async fn drain_projection<S: McpSessionSlots>(
        &self,
        slots: &S,
        generation: &awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), McpError> {
        let Some(drain) = slots
            .read_mcp(generation.session_id().as_str(), |mcp| {
                mcp.projections
                    .iter()
                    .find(|projection| projection.request.generation == *generation)
                    .map(|projection| projection.drain.clone())
            })
            .flatten()
        else {
            return Ok(());
        };
        // Keep the exact owner installed while awaiting. Cancellation releases
        // only this serialization guard; a retry sees Draining plus the same
        // process/activity and resumes the canonical cleanup.
        let _drain = drain.lock().await;
        let projected =
            slots.modify_mcp_and_runtime(generation.session_id().as_str(), |mcp, runtime| {
                let Some(projection) = mcp
                    .projections
                    .iter_mut()
                    .find(|projection| projection.request.generation == *generation)
                else {
                    // Cleanup is an idempotent exact-generation command. A fresh
                    // Runtime incarnation legitimately has no process-local copy of
                    // an already fenced durable Draining generation.
                    return Ok::<_, McpError>(None);
                };
                if !Arc::ptr_eq(&projection.drain, &drain) {
                    return Err(McpError::unavailable_classified(
                        "mcp_generation_owner_changed",
                        "MCP generation projection was replaced while cleanup was waiting",
                    ));
                }
                if projection.state == McpProjectionState::Removed {
                    return Ok::<_, McpError>(None);
                }
                // Visibility closes before cancellation. Existing Runtime clones may
                // still hold the dynamic tools, so their exact transport fences are
                // cancelled and drained below before Removed is acknowledged.
                projection.state = McpProjectionState::Draining;
                let call_fences = projection
                    .native_wiring
                    .as_ref()
                    .map(|wiring| wiring.call_fences.clone())
                    .unwrap_or_default();
                let process = projection.mcp_process.clone();
                let staging = projection.staging.clone();
                // The canonical Session slot must retain this Runtime owner across
                // cancellation and timeout. Only the outer environment-quiescence
                // transaction clears it after every MCP generation and the Hand
                // have proved quiescent; otherwise a retry could lose the active-run
                // fence and manufacture a Removed proof while the old Run is live.
                let runtime = runtime.clone();
                Ok(Some((call_fences, process, staging, runtime)))
            });
        let Some((mut call_fences, process, staging, runtime)) = projected.transpose()?.flatten()
        else {
            return Ok(());
        };

        // WorkerRelay requests are not Runtime tool calls: Axum clones the
        // exact Route before it awaits the body/upstream/stream. Add that
        // route owner's existing MCP call fence to the same drain barrier.
        // Removing the map entry is not quiescence because an accepted clone
        // can still hold its credential and upstream future.
        if let Some(relay) = self.relay.get()
            && let Some(fence) = relay.route_call_fence(generation)
        {
            call_fences.push(fence);
        }
        // Poll every exact fence together so all admission closes before a
        // single slow permit can consume the bounded settlement window.
        tokio::time::timeout(
            MCP_GENERATION_CALL_QUIESCENCE_TIMEOUT,
            futures_util::future::join_all(call_fences.iter().map(|fence| fence.close_and_wait())),
        )
        .await
        .map_err(|_| {
            McpError::unavailable_classified(
                "mcp_generation_call_quiescence_timeout",
                "MCP generation drain could not quiesce an accepted MCP call",
            )
        })?;

        let had_staging = staging.is_some();
        if let Some(staging) = staging {
            staging.wait().await;
        }
        // Spawn may have completed after Draining was installed but before the
        // activity barrier closed. The task transfers that process into this
        // same projection; re-read it before reaping so no post-snapshot child
        // can be erased as an unowned handle.
        let process = if process.is_none() && had_staging {
            slots
                .read_mcp(generation.session_id().as_str(), |mcp| {
                    mcp.projections
                        .iter()
                        .find(|projection| projection.request.generation == *generation)
                        .and_then(|projection| projection.mcp_process.clone())
                })
                .flatten()
        } else {
            process
        };
        // A missing cleanup authority may close visibility but must not
        // cancel the active Run and then manufacture a Removed process proof.
        // The exact Arc remains in Draining for a later retry.
        let process_reaper = process
            .as_ref()
            .map(|_| self.process_reaper())
            .transpose()?;

        if let Some(runtime) = runtime {
            S::cancel_runtime(&runtime);
            let deadline =
                awaken_time::mono::MonoDeadline::after(std::time::Duration::from_secs(10));
            loop {
                if !S::runtime_has_active_run(&runtime) {
                    break;
                }
                if deadline.has_passed() {
                    return Err(McpError::unavailable_classified(
                        "mcp_generation_quiescence_timeout",
                        "MCP generation drain could not quiesce the active Session Run",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        if let Some(process) = process {
            process_reaper
                .expect("a tracked MCP process required the installed reaper")
                .reap(process.as_ref(), std::time::Duration::from_secs(5))
                .await
                .map_err(|error| {
                    McpError::unavailable_classified(
                        "mcp_generation_process_reap_failed",
                        format!("MCP generation process did not terminate: {error}"),
                    )
                })?;
        }

        // This is the sole visible-route removal edge. It runs only after the
        // exact route fence, Runtime, staging, and process owners are quiescent;
        // cancellation or any failure above leaves the closed Route installed
        // with the Draining projection so retry retains the same proof owner.
        if let Some(relay) = self.relay.get() {
            relay.remove_route(generation);
        }

        match slots.modify_mcp(generation.session_id().as_str(), |mcp| {
            let Some(projection) = mcp
                .projections
                .iter_mut()
                .find(|projection| projection.request.generation == *generation)
            else {
                return Ok(());
            };
            if projection.state == McpProjectionState::Removed {
                return Ok(());
            }
            if projection.state != McpProjectionState::Draining {
                return Err(McpError::internal(
                    "MCP generation changed while its drain was quiescing",
                ));
            }
            projection.server = None;
            projection.native_wiring = None;
            projection.mcp_process = None;
            projection.staging = None;
            projection.state = McpProjectionState::Removed;
            Ok(())
        }) {
            Some(result) => result,
            None => Ok(()),
        }
    }

    /// Drain every exact durable generation requested by the Session owner and
    /// any additional local leak, continuing after individual failures. The
    /// returned proof echoes the durable expected set; local slot contents are
    /// used only to find additional cleanup work and to reject residual effects.
    pub async fn drain_projections<S: McpSessionSlots>(
        &self,
        slots: &S,
        thread: &str,
        expected: &[awaken_session_mcp_contract::McpGenerationRef],
    ) -> Result<McpQuiescenceProof, McpError> {
        let mut targets = Vec::with_capacity(expected.len());
        for generation in expected {
            if generation.session_id() != thread {
                return Err(McpError::internal(
                    "MCP quiescence generation belongs to another Session",
                ));
            }
            if targets.iter().any(|seen| seen == generation) {
                return Err(McpError::internal(
                    "MCP quiescence expected set contains a duplicate generation",
                ));
            }
            targets.push(generation.clone());
        }
        for generation in slots
            .read_mcp(thread, |mcp| {
                mcp.projections
                    .iter()
                    .filter(|projection| projection.state != McpProjectionState::Removed)
                    .map(|projection| projection.request.generation.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        {
            if !targets.iter().any(|seen| seen == &generation) {
                targets.push(generation);
            }
        }

        let mut first_error = None;
        for generation in &targets {
            if let Err(error) = self.drain_projection(slots, generation).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        let residual = slots
            .read_mcp(thread, |mcp| {
                mcp.projections
                    .iter()
                    .any(|projection| projection.state != McpProjectionState::Removed)
            })
            .unwrap_or(false);
        if let Some(error) = first_error {
            return Err(error);
        }
        if residual {
            return Err(McpError::unavailable_classified(
                "mcp_generation_quiescence_incomplete",
                "MCP generation cleanup left a process-local owner unresolved",
            ));
        }
        Ok(McpQuiescenceProof {
            generations: expected.to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpQuiescenceProof {
    pub generations: Vec<awaken_session_mcp_contract::McpGenerationRef>,
}
