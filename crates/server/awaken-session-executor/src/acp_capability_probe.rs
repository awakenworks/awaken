// @formal surface obligation=worker.acp_capability_is_detected_only_after_a_verified_handshake
// @formal reason: Session-Environment adapter for one bounded ACP capability probe. Its scope is derived
// @formal reason: from the runtime owner plus the probe inputs rather than generated, so a cancelled probe
// @formal reason: names the exact resource without granting cleanup authority, and the probe
// @formal reason: is wrapped in one responsibility claim minted before the first poll. It observes
// @formal reason: capability only through a completed handshake and never publishes evidence of its own;
// @formal reason: this is not a direct formal-proof claim.
//! ACP capability negotiation through the already-selected Session Environment.
//!
//! The Worker ACP application remains the observation owner. This adapter only
//! opens a prompt-free process/channel in the same Namespace/Container provider
//! that later realizes published Runs, then tears the probe environment down.
//!
//! A scope derived from owner/argv/auth makes interrupted resources discoverable.
//! It does not confer adoption or deletion authority after process loss.
//! The Host task registry retains each live handle through caller cancellation;
//! a later process still needs the provider’s durable effect and physical proof.

use awaken_session_realization::peers::EnvironmentPeer as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityNegotiator, AcpCapabilityProbeConfig,
    NegotiatedAcpCapabilities,
};
use awaken_provisioning_contract as pc;

/// The reader a probe claim names.
///
/// Each tier's reconciliation reads the stamp back with its own scan —
/// `DockerRuntime::existing_realizations`, the Kubernetes object lookup,
/// `observe_adoption` on the filesystem tiers — so naming one of them here
/// would name the wrong one everywhere else. What every scan has in common is
/// that it needs the scope, and [`SessionAcpCapabilityNegotiator::probe_scope`]
/// is the function that recovers it from inputs that outlive any task.
const PROBE_RECONCILER: &str = "SessionAcpCapabilityNegotiator::probe_scope";

/// Probe scopes whose environment may still stand with nobody in this process
/// left to dispose it.
///
/// This is an index, not the record: the record is the scope stamped on the
/// resource itself, and the scope is recomputable from scratch, so losing this
/// ledger to a restart costs a shortcut and nothing more. It mirrors
/// `awaken_sandbox_local::AbandonedRealizations`, which stands in the same
/// relation to the filesystem realization marker.
///
/// Crate-visible, not public: nothing outside this crate consumes it yet, and
/// widening the published surface ahead of a consumer is how a narrow API stops
/// being narrow. Promoting it is a one-word change when an operator endpoint
/// wants to read the ledger.
#[derive(Default)]
pub(crate) struct StrandedProbeEnvironments {
    scopes: Mutex<Vec<String>>,
}

impl StrandedProbeEnvironments {
    /// The retention handed to [`pc::EffectClaim::durable`]: it records `scope`
    /// if and only if the claimed probe is dropped before it settles. Built
    /// before the future, because a retention constructed afterwards would be
    /// exactly the ordering the claim exists to forbid.
    fn retain(self: &Arc<Self>, scope: String) -> impl FnOnce() + Send + 'static {
        let ledger = Arc::clone(self);
        move || ledger.record(scope)
    }

    fn record(&self, scope: String) {
        let Some(mut scopes) = self.lock() else {
            return;
        };
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }

    /// The environment at `scope` is provably gone, so the strand is over.
    fn settle(&self, scope: &str) {
        if let Some(mut scopes) = self.lock() {
            scopes.retain(|recorded| recorded != scope);
        }
    }

    /// Whether this process believes `scope` still has an environment standing.
    #[must_use]
    fn contains(&self, scope: &str) -> bool {
        self.lock()
            .is_some_and(|scopes| scopes.iter().any(|recorded| recorded == scope))
    }

    /// The ledger, unless its lock is poisoned. The ledger is an index whose
    /// loss costs only a diagnostic shortcut (the record is the scope stamped
    /// on the resource), so a poisoned ledger reads exactly like the empty
    /// ledger a restart leaves:
    ///
    /// | Rule | ledger lock | `record` / `settle` | `contains` |
    /// |---|---|---|---|
    /// | S1 | healthy  | updates the index | the index's answer |
    /// | S2 | poisoned | no effect          | `false` (no strand hint in the create error) |
    ///
    /// S2 decides no lifecycle: reconciliation reads the stamped scope, never
    /// this index, so no environment is disposed or kept because of it.
    fn lock(&self) -> Option<std::sync::MutexGuard<'_, Vec<String>>> {
        self.scopes.lock().ok()
    }
}

#[derive(Clone)]
pub struct SessionAcpCapabilityNegotiator {
    probe: AcpProbe,
    background: Arc<awaken_session_run_delivery::BackgroundRuns>,
    scope_owner: String,
}

/// Only the selected execution inputs cross into the owned task. In particular,
/// the task does not retain its own background supervisor or Session owners.
#[derive(Clone)]
struct AcpProbe {
    provider: crate::session_environment::SessionEnvironmentProvider,
    timeout: Duration,
    handshake: Arc<dyn AcpCapabilityHandshake>,
    stranded: Arc<StrandedProbeEnvironments>,
}

impl SessionAcpCapabilityNegotiator {
    #[must_use]
    pub fn new(
        assembly: Arc<crate::RunAssembly>,
        scope_owner: String,
        timeout: Duration,
        handshake: Arc<dyn AcpCapabilityHandshake>,
    ) -> Self {
        Self {
            probe: AcpProbe {
                provider: assembly
                    .realization_executor
                    .environments()
                    .providers()
                    .session_provider
                    .clone(),
                timeout,
                handshake,
                stranded: Arc::default(),
            },
            background: assembly.resources.memory.background(),
            scope_owner,
        }
    }

    /// The one scope a probe of `argv` under `owner` uses, on every tier and in
    /// every process.
    ///
    /// `owner` is the injected runtime incarnation coordinate, so two
    /// runtimes sharing a container daemon or a Kubernetes namespace cannot
    /// adopt each other's probe. The rest is exactly the probe's inputs: argv
    /// decides which CLI is launched and the auth method decides which
    /// handshake it performs, so two probes that differ in either are different
    /// effects and must not share an environment.
    #[must_use]
    pub(crate) fn probe_scope(
        owner: &str,
        argv: &[String],
        auth_method_id: Option<&str>,
    ) -> String {
        let digest = awaken_agent_contract::fingerprint::canonical_json_sha256(
            "awaken.acp.capability-probe.v1",
            &(owner, argv, auth_method_id),
        )
        .expect("probe inputs are strings and always serialize");
        // A scope becomes a container label, a Kubernetes object name and a
        // filesystem leaf, so keep it to the intersection: lowercase hex with
        // a readable prefix, well under every tier's length limit.
        let hex = digest.trim_start_matches("sha256:");
        format!("acp-capability-probe-{}", &hex[..32])
    }
}

impl AcpProbe {
    /// Create, handshake, kill, dispose. Everything from the first physical
    /// effect to the last is inside this one future, so the claim that wraps it
    /// covers every cancellation point rather than only the create.
    async fn probe(
        &self,
        scope: &str,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
        response: &mut tokio::sync::oneshot::Sender<
            Result<NegotiatedAcpCapabilities, pc::SandboxError>,
        >,
    ) -> Result<NegotiatedAcpCapabilities, pc::SandboxError> {
        let spec = crate::sandbox_layout::agent_run_sandbox_spec(scope);
        let environment = self.provider.create(&spec).await.map_err(|error| {
            // A derived scope means a failure here is usually the *previous*
            // probe's strand occupying it. Say so: "the root is not absent"
            // sends an operator hunting, the scope plus the reason sends
            // them straight at the one environment to reconcile.
            let strand = if self.stranded.contains(scope) {
                format!(
                    "; an earlier probe at scope `{scope}` was interrupted and its \
                         environment was never disposed"
                )
            } else {
                String::new()
            };
            error.map_detail(|detail| {
                format!("create ACP capability probe environment: {detail}{strand}")
            })
        })?;
        let result = async {
            if response.is_closed() {
                return Err(pc::SandboxError::timeout("ACP capability probe caller cancelled"));
            }
            let (process, mut channel) = environment
                .agent_process()
                .spawn_agent(pc::Command {
                    argv: argv.to_vec(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: Vec::new(),
                    stdio: pc::Stdio::Piped,
                })
                .await
                .map_err(|error| {
                    error.map_detail(|detail| format!("spawn ACP capability probe: {detail}"))
                })?;
            let config = AcpCapabilityProbeConfig {
                session_cwd: Some(cwd.to_string_lossy().into_owned()),
                auth_method_id: auth_method_id.map(str::to_string),
            };
            let handshake = tokio::time::timeout(
                self.timeout,
                self.handshake.negotiate(channel.as_mut(), &config),
            );
            let negotiated = tokio::select! {
                biased;
                () = response.closed() => Err(pc::SandboxError::timeout("ACP capability probe caller cancelled")),
                result = handshake => result
            .map_err(|_| pc::SandboxError::timeout("ACP capability probe timed out"))
            .and_then(|result| {
                result
                    .map_err(|error| pc::SandboxError::backend("negotiate ACP capabilities", error))
            })
            };
            drop(channel);
            // A runtime signal is external I/O. If it stalls, proceeding to
            // environment disposal is the only path that can still reap the
            // container and release Worker capacity.
            let _ = tokio::time::timeout(Duration::from_secs(2), process.signal(pc::Signal::Kill))
                .await;
            let _ = tokio::time::timeout(Duration::from_secs(2), process.wait()).await;
            negotiated
        }
        .await;
        // Disposal itself crosses the container runtime boundary. Bound it so
        // a wedged daemon cannot hold the capability refresh (and therefore a
        // Worker heartbeat/lease) forever.
        let dispose =
            tokio::time::timeout(Duration::from_secs(10), environment.lifecycle().dispose())
                .await
                .map_err(|_| {
                    pc::SandboxError::timeout("dispose ACP capability probe environment timed out")
                })
                .and_then(|result| {
                    result.map_err(|error| {
                        error.map_detail(|detail| {
                            format!("dispose ACP capability probe environment: {detail}")
                        })
                    })
                });
        // A disposal that did not prove the environment gone is a strand this
        // process *observed*, not one a cancellation hid; record it directly
        // rather than waiting for a claim that is about to settle.
        if dispose.is_err() {
            self.stranded.record(scope.to_string());
        } else {
            self.stranded.settle(scope);
        }
        match (result, dispose) {
            (Ok(negotiated), Ok(())) => Ok(negotiated),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

#[async_trait]
impl AcpCapabilityNegotiator for SessionAcpCapabilityNegotiator {
    async fn negotiate(
        &self,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        if self.scope_owner.trim().is_empty() {
            return Err("ACP probe requires its runtime incarnation discovery coordinate".into());
        }
        let scope = Self::probe_scope(&self.scope_owner, argv, auth_method_id);
        // The claim is minted from the scope and the ledger before the probe
        // future is built, so nothing can be polled — and therefore no
        // environment or process can exist — before a record names it. The
        // durable half of that record is the scope the provider stamps on the
        // resource; the retention covers the window before that stamp lands,
        // which the stamp itself cannot.
        let claim =
            pc::EffectClaim::durable(PROBE_RECONCILER, self.probe.stranded.retain(scope.clone()));
        let (mut response, result) = tokio::sync::oneshot::channel();
        let probe = self.probe.clone();
        let argv = argv.to_vec();
        let cwd = cwd.to_owned();
        let auth_method_id = auth_method_id.map(str::to_owned);
        // This is the Host's existing drainable task registry, shared with
        // auxiliary execution. Receiver cancellation may interrupt handshake,
        // but cannot drop a create response or the exact-handle cleanup tail.
        self.background
            .spawn(
                awaken_session_run_delivery::BackgroundWorkClass::UnattachedEnvironment,
                async move {
                    let outcome = pc::ClaimedEffect::new(
                        claim,
                        probe.probe(
                            &scope,
                            &argv,
                            &cwd,
                            auth_method_id.as_deref(),
                            &mut response,
                        ),
                    )
                    .await;
                    let _ = response.send(outcome);
                },
            )
            .await;
        result
            .await
            .map_err(|_| "ACP capability probe owner ended before cleanup completed".to_string())?
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "acp_capability_probe/tests.rs"]
mod tests;
