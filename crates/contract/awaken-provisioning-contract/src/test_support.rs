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
