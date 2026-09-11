//! Explicit, revocable authority fixture shared by provider contract tests.
//! This is not a production clock or an authority inferred from fence expiry.

use crate::{CurrentExecutionAuthority, SandboxEffectFence};
use std::{io, sync::Mutex, time::Duration};

pub struct EffectAuthority {
    pub current: Mutex<Option<(SandboxEffectFence, Duration)>>,
}

impl EffectAuthority {
    pub fn new(fence: &SandboxEffectFence) -> Self {
        Self {
            current: Mutex::new(Some((fence.clone(), Duration::from_secs(1)))),
        }
    }

    pub fn denied() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }
}

impl CurrentExecutionAuthority for EffectAuthority {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        let current = self.current.lock().unwrap();
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
        let current = self.current.lock().unwrap();
        let (fence, remaining) = current.as_ref().ok_or(io::ErrorKind::PermissionDenied)?;
        if !asserted.same_effect_identity(fence)
            || asserted.expires_at_unix_ms > fence.expires_at_unix_ms
            || remaining.is_zero()
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        effect(fence, *remaining)
    }
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
        fence.owner.clone(),
        fence.runtime_incarnation.clone(),
        fence.epoch,
        fence.expires_at_unix_ms,
    )?;
    let authorization = preparation.authorize(successor.clone())?;
    let successor_authority = EffectAuthority::new(&successor);
    sandbox
        .dispose_for_effect(&authorization, &successor_authority)
        .await
}
