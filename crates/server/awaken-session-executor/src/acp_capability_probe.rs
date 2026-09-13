//! ACP capability negotiation through the already-selected Session Environment.
//!
//! The Worker ACP application remains the observation owner. This adapter only
//! opens a prompt-free process/channel in the same Namespace/Container provider
//! that later realizes published Runs, then tears the probe environment down.
//!
//! # Why the probe scope is derived rather than generated
//!
//! The probe creates a real environment and a real agent process, and a
//! cancelled probe strands both. Whether that is recoverable comes down to one
//! thing: can anything else *name* what was left behind. Every tier reconciles
//! by the exact scope the provider stamped on the resource — the
//! `SANDBOX_SCOPE_LABEL` a container backend selects on, the deterministic
//! object name the Kubernetes backend discovers by, the sandbox root and its
//! creation record on the filesystem tiers — so a scope drawn from a fresh
//! random UUID is a name that dies with the cancelled task. Nothing on any tier
//! can ever select on it again.
//!
//! So the scope is a pure function of who is probing and what is being probed.
//! A repeat probe of the same CLI under the same runtime owner computes the
//! same scope, which makes a stranded environment both *findable* (a scan has a
//! selector) and *reclaimable* (the next probe lands on the same environment
//! rather than beside it). Two different CLIs, or two runtime owners sharing a
//! provider, still never collide.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityNegotiator, AcpCapabilityProbeConfig,
    NegotiatedAcpCapabilities,
};
use awaken_provisioning_contract as pc;

use crate::SharedHost;

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
        let mut scopes = self.lock();
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }

    /// The environment at `scope` is provably gone, so the strand is over.
    fn settle(&self, scope: &str) {
        self.lock().retain(|recorded| recorded != scope);
    }

    /// Whether this process believes `scope` still has an environment standing.
    #[must_use]
    fn contains(&self, scope: &str) -> bool {
        self.lock().iter().any(|recorded| recorded == scope)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.scopes.lock().expect("stranded probe ledger poisoned")
    }
}

pub struct SessionAcpCapabilityNegotiator {
    host: Arc<SharedHost>,
    timeout: Duration,
    handshake: Arc<dyn AcpCapabilityHandshake>,
    stranded: Arc<StrandedProbeEnvironments>,
}

impl SessionAcpCapabilityNegotiator {
    #[must_use]
    pub fn new(
        host: Arc<SharedHost>,
        timeout: Duration,
        handshake: Arc<dyn AcpCapabilityHandshake>,
    ) -> Self {
        Self {
            host,
            timeout,
            handshake,
            stranded: Arc::default(),
        }
    }

    /// The one scope a probe of `argv` under `owner` uses, on every tier and in
    /// every process.
    ///
    /// `owner` is the runtime owner's persisted workspace coordinate, so two
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

    /// Create, handshake, kill, dispose. Everything from the first physical
    /// effect to the last is inside this one future, so the claim that wraps it
    /// covers every cancellation point rather than only the create.
    async fn probe(
        &self,
        scope: &str,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
    ) -> Result<NegotiatedAcpCapabilities, pc::SandboxError> {
        let fail = pc::SandboxError::new;
        let spec = crate::provisioning::agent_run_sandbox_spec(scope);
        let environment = self
            .host
            .session_provider
            .create(&spec)
            .await
            .map_err(|error| {
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
                fail(format!(
                    "create ACP capability probe environment: {error}{strand}"
                ))
            })?;
        let result = async {
            let (process, mut channel) = environment
                .spawn_agent(pc::Command {
                    argv: argv.to_vec(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: Vec::new(),
                    stdio: pc::Stdio::Piped,
                })
                .await
                .map_err(|error| format!("spawn ACP capability probe: {error}"))?;
            let config = AcpCapabilityProbeConfig {
                session_cwd: Some(cwd.to_string_lossy().into_owned()),
                auth_method_id: auth_method_id.map(str::to_string),
            };
            let negotiated = tokio::time::timeout(
                self.timeout,
                self.handshake.negotiate(channel.as_mut(), &config),
            )
            .await
            .map_err(|_| "ACP capability probe timed out".to_string())
            .and_then(|result| result);
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
        let dispose = tokio::time::timeout(Duration::from_secs(10), environment.dispose())
            .await
            .map_err(|_| "dispose ACP capability probe environment timed out".to_string())
            .and_then(|result| {
                result.map_err(|error| format!("dispose ACP capability probe environment: {error}"))
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
            (Err(error), _) | (Ok(_), Err(error)) => Err(fail(error)),
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
        let scope = Self::probe_scope(self.host.local_workspace(), argv, auth_method_id);
        // The claim is minted from the scope and the ledger before the probe
        // future is built, so nothing can be polled — and therefore no
        // environment or process can exist — before a record names it. The
        // durable half of that record is the scope the provider stamps on the
        // resource; the retention covers the window before that stamp lands,
        // which the stamp itself cannot.
        let claim = pc::EffectClaim::durable(PROBE_RECONCILER, self.stranded.retain(scope.clone()));
        pc::ClaimedEffect::new(claim, self.probe(&scope, argv, cwd, auth_method_id))
            .await
            .map_err(|error| error.0)
    }
}

#[cfg(test)]
#[path = "acp_capability_probe/tests.rs"]
mod tests;
