// @formal surface exclude=test_only
// @formal owner crates/contract/awaken-provisioning-contract/src/lib.rs
// @formal owner crates/contract/awaken-provisioning-contract/Cargo.toml
// @formal reason: Static owner: one explicit revocable effect-authority fixture shared by provider tests.
// @formal reason: Dynamic path: tests install, renew or revoke its finite grant; the module is compiled
// @formal reason: only by cfg(any(test, feature = test-support)), not the default production graph. It is
// @formal reason: not a production clock or a second Session authority.
//! Explicit, revocable authority fixture shared by provider contract tests.
//! This is not a production clock or an authority inferred from fence expiry.

use crate::{CurrentExecutionAuthority, SandboxEffectFence};
use awaken_sync::{InvariantLock, LeafLock};
use std::{io, time::Duration};

pub struct EffectAuthority {
    pub current: InvariantLock<Option<(SandboxEffectFence, Duration)>>,
}

impl EffectAuthority {
    pub fn new(fence: &SandboxEffectFence) -> Self {
        Self {
            current: InvariantLock::new(Some((fence.clone(), Duration::from_secs(1)))),
        }
    }

    pub fn denied() -> Self {
        Self {
            current: InvariantLock::new(None),
        }
    }
}

impl CurrentExecutionAuthority for EffectAuthority {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        // A poisoned authority grants nothing.
        let current = self
            .current
            .lock()
            .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        let (_, remaining) = current.as_ref().ok_or(io::ErrorKind::PermissionDenied)?;
        if remaining.is_zero() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        effect(*remaining)
    }

    fn with_current_effect(
        &self,
        asserted: &SandboxEffectFence,
        effect: &mut dyn FnMut(&SandboxEffectFence, Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        // A poisoned authority grants nothing.
        let current = self
            .current
            .lock()
            .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        let (fence, remaining) = current.as_ref().ok_or(io::ErrorKind::PermissionDenied)?;
        if !asserted.same_effect_identity(fence)
            || asserted.expires_at_unix_ms() > fence.expires_at_unix_ms()
            || remaining.is_zero()
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        effect(fence, *remaining)
    }
}

/// Every fake effect whose caller went away before it settled.
///
/// A test double creates nothing on a host, but it still owes the same answer
/// as a real provider to "who is responsible for this?" — so it mints
/// [`crate::EffectClaim::in_process`] against this ledger and the abandonment
/// becomes as observable in a fixture as a stranded container is in production.
/// Process-wide rather than per-double so a fake can stay a plain value that
/// dozens of call sites construct positionally.
static ABANDONED_EFFECTS: LeafLock<Vec<String>> = LeafLock::new(Vec::new());

/// The retention a test double hands to its claim: it records `identity` if and
/// only if the claimed effect is dropped unsettled.
pub fn retain_abandoned_effect(identity: impl Into<String>) -> impl FnOnce() + Send + 'static {
    let identity = identity.into();
    move || {
        ABANDONED_EFFECTS.lock().push(identity);
    }
}

/// Take everything the ledger has recorded so far, leaving it empty. Tests that
/// assert on abandonment drain it; tests that do not are unaffected, because an
/// effect that settles never records anything here in the first place.
#[must_use]
pub fn drain_abandoned_effects() -> Vec<String> {
    std::mem::take(&mut *ABANDONED_EFFECTS.lock())
}

/// A live fence plus its matching authority, for tests that own no aggregate
/// effect yet must still drive the provider's fenced obligations. Production
/// callers always carry a real Session-owned effect; this exists so a test
/// never reaches for an unfenced shortcut that no longer exists.
#[must_use]
pub fn fixture_effect(operation_id: &str) -> (SandboxEffectFence, EffectAuthority) {
    let fence = SandboxEffectFence::new(
        operation_id,
        "fixture-owner",
        "fixture-runtime",
        1,
        u64::MAX,
    )
    .expect("fixture effect fence is well formed");
    let authority = EffectAuthority::new(&fence);
    (fence, authority)
}

/// Reclaim an environment that a fixture created through the fenced edge.
///
/// Such a sandbox has a current realization, so the provider correctly refuses
/// [`crate::Sandbox::dispose_unattached`] for it: unattached reclaim is for
/// capacity nothing owns. Tests therefore drive the same prepared/authorized
/// pair production uses.
pub async fn dispose_owned_fixture(
    sandbox: &dyn crate::Sandbox,
    fence: &SandboxEffectFence,
    authority: &EffectAuthority,
) -> Result<(), crate::SandboxError> {
    let prepared = sandbox
        .prepare_disposal_for_effect(fence, authority)
        .await?;
    let preparation = crate::SandboxDisposalPreparation::new(prepared, "fixture-prepared")?;
    let successor = SandboxEffectFence::new(
        preparation.operation_id()?,
        fence.owner(),
        fence.runtime_incarnation(),
        fence.epoch(),
        fence.expires_at_unix_ms(),
    )?;
    let authorization = preparation.authorize(successor.clone())?;
    let successor_authority = EffectAuthority::new(&successor);
    sandbox
        .dispose_for_effect(&authorization, &successor_authority)
        .await
}

/// Opaque locator of the neutral fixture provider. It deliberately mirrors no
/// real backend: contract consumers exercise only the neutral handle evidence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureHandleLocator {
    pub outputs_path: String,
    pub physical_incarnation: String,
}

#[derive(serde::Serialize)]
struct FixtureCurrentPayload<'a> {
    previous: &'a FixtureHandleLocator,
    realization_fingerprint: &'a crate::SandboxRealizationFingerprint,
    effect_fence: &'a SandboxEffectFence,
    owned_paths: &'a [String],
}

/// Provider name of [`fixture_current_handle`] / [`fixture_legacy_handle`].
pub const FIXTURE_HANDLE_PROVIDER: &str = "fixture";

/// A current (evidence-carrying) handle of the neutral fixture provider.
#[must_use]
pub fn fixture_current_handle(
    sandbox_id: impl Into<String>,
    locator: &FixtureHandleLocator,
    realization_fingerprint: &crate::SandboxRealizationFingerprint,
    effect_fence: &SandboxEffectFence,
    owned_paths: &[String],
) -> crate::SandboxHandle {
    crate::SandboxHandle::from_provider_payload(
        sandbox_id,
        crate::SandboxPayloadSchema::new(FIXTURE_HANDLE_PROVIDER, 2)
            .expect("fixture schema is well formed"),
        &FixtureCurrentPayload {
            previous: locator,
            realization_fingerprint,
            effect_fence,
            owned_paths,
        },
    )
    .expect("fixture payload carries complete neutral evidence")
}

/// A legacy (evidence-free) handle of the neutral fixture provider.
#[must_use]
pub fn fixture_legacy_handle(
    sandbox_id: impl Into<String>,
    locator: &FixtureHandleLocator,
) -> crate::SandboxHandle {
    crate::SandboxHandle::from_provider_payload(
        sandbox_id,
        crate::SandboxPayloadSchema::new(FIXTURE_HANDLE_PROVIDER, 1)
            .expect("fixture schema is well formed"),
        locator,
    )
    .expect("fixture legacy payload is one object")
}

/// Tamper one persisted coordinate of a fixture's fence in place, through the
/// fence's one admission. Consumers: provider decision tables that advance,
/// renew or corrupt the current authority of an [`EffectAuthority`] fixture
/// row by row.
pub trait FenceTamper {
    fn tamper(&mut self, edit: impl FnOnce(&mut crate::SandboxEffectFenceWire));

    /// Extend expiry by one millisecond: a renewal of the same tenure.
    fn renew(&mut self) {
        self.tamper(|wire| wire.expires_at_unix_ms += 1);
    }

    /// Advance to the next epoch under the same holder: a reassignment.
    fn reassign(&mut self) {
        self.tamper(|wire| wire.epoch += 1);
    }
}

impl FenceTamper for SandboxEffectFence {
    fn tamper(&mut self, edit: impl FnOnce(&mut crate::SandboxEffectFenceWire)) {
        *self = self
            .with_persisted(edit)
            .expect("a tampered fixture fence stays admissible");
    }
}

// Fixture-input partitions: F1 Sandbox admission uses Workdir + Some(60);
// F2 adoption/effect tables use their original scope + None; F3 the namespace
// admission setup adds its readonly file/TZ/allowlist; F4 restoration setup
// retains its own outputs/agent command. Those existing tests own outcomes.
// This one DTO initializer creates no environment, claim, clock or authority.
#[cfg(test)]
pub(crate) fn spec(scope: &str, lease_ttl_secs: Option<u64>) -> crate::SandboxSpec {
    crate::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: crate::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: crate::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: crate::FilesystemContinuity::Retained,
        lease_ttl_secs,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}
