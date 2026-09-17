//! Borrow current authority around the existing physical creation participant.
//! No new lease or persisted state: marker identity and phase remain in `super`.
use super::*;

pub(crate) enum ProviderCreationGuard<'a> {
    Current {
        guard: Box<CreationGuard>,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    },
    Legacy(LegacyCreationGuard),
}

impl<'a> ProviderCreationGuard<'a> {
    pub(crate) fn current(
        root: &Path,
        fingerprint: &pc::SandboxRealizationFingerprint,
        fence: &pc::SandboxEffectFence,
        source: Option<RebuildSource<'_>>,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    ) -> Result<Self, pc::SandboxError> {
        pc::with_current_sandbox_effect(fence, authority, |_| {
            Ok(Self::Current {
                guard: Box::new(begin(root, fingerprint, fence, source, None)?),
                authority,
            })
        })
    }

    /// Current proof only: also usable after Ready publication released the
    /// physical lock, e.g. for confirming the retained Memory response.
    pub(crate) fn with_authority<T>(
        &self,
        apply: impl FnOnce() -> Result<T, pc::SandboxError>,
    ) -> Result<T, pc::SandboxError> {
        match self {
            Self::Current { guard, authority } => {
                pc::with_current_sandbox_effect(&guard.marker.effect_fence, *authority, |_| apply())
            }
            Self::Legacy(_) => apply(),
        }
    }

    pub(crate) fn with_mutation<T>(
        &self,
        apply: impl FnOnce() -> Result<T, pc::SandboxError>,
    ) -> Result<T, pc::SandboxError> {
        self.with_authority(|| {
            match self {
                Self::Current { guard, .. } => guard.validate_before_mutation()?,
                Self::Legacy(guard) => guard.validate_before_mutation()?,
            }
            apply()
        })
    }

    pub(crate) fn is_incomplete(&self) -> bool {
        match self {
            Self::Current { guard, .. } => guard.admission != Admission::Ready,
            Self::Legacy(_) => true,
        }
    }
    pub(crate) fn completed_receipt(
        &self,
    ) -> Result<Option<&RealizationCompletionReceipt>, pc::SandboxError> {
        match self {
            Self::Current { guard, .. } => guard.completed_receipt(),
            Self::Legacy(_) => Ok(None),
        }
    }
    pub(crate) fn root_identity(&self) -> Result<Option<DirectoryIdentity>, pc::SandboxError> {
        match self {
            Self::Current { guard, .. } => required_root_identity(&guard.marker).map(Some),
            Self::Legacy(guard) => Ok(guard.identity),
        }
    }
    pub(crate) fn prepare_root(&mut self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Current { guard, authority } => pc::with_current_sandbox_effect(
                &guard.marker.effect_fence.clone(),
                *authority,
                |admitted| guard.prepare_root(admitted),
            ),
            Self::Legacy(guard) => guard.prepare_root(),
        }
    }
    pub(crate) fn validate_before_mutation(&self) -> Result<(), pc::SandboxError> {
        self.with_mutation(|| Ok(()))
    }
    pub(crate) fn complete(
        &mut self,
        receipt: &RealizationCompletionReceipt,
    ) -> Result<LiveRealization, pc::SandboxError> {
        match self {
            Self::Current { guard, authority } => pc::with_current_sandbox_effect(
                &guard.marker.effect_fence.clone(),
                *authority,
                |admitted| {
                    guard
                        .complete(receipt, admitted)
                        .map(LiveRealization::Current)
                },
            ),
            Self::Legacy(guard) => guard.complete().map(LiveRealization::LegacyCreated),
        }
    }
    pub(crate) fn validate_memory_replay(&self) -> Result<(), pc::SandboxError> {
        self.with_mutation(|| {
            if let Self::Current { guard, .. } = self {
                execution_participant::require_quiescent(&guard.marker)?;
            }
            Ok(())
        })
    }
    /// State that this creation's compensation could not finish, so the root
    /// it left is retained deliberately rather than abandoned silently. The
    /// fenced guard already says this with its own preserved marker phase; the
    /// unfenced one has only its creation record to say it with.
    pub(crate) fn retain_failed_compensation(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Current { .. } => Ok(()),
            Self::Legacy(guard) => guard.retain_failed_compensation(),
        }
    }

    pub(crate) fn abort_creation(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Current { guard, authority } => pc::with_current_sandbox_effect(
                &guard.marker.effect_fence.clone(),
                authority,
                |admitted| (*guard).abort_creation(admitted),
            ),
            Self::Legacy(guard) => guard.abort_creation(),
        }
    }
}
