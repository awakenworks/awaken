//! Backend capability metadata.

use super::{
    BackendDelegateContinuation, BackendDelegatePersistence, BackendDelegateRunRequest,
    BackendRootRunRequest, ExecutionBackendError,
};

/// How a backend can be interrupted after execution starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCancellationCapability {
    None,
    CooperativeToken,
    RemoteAbort,
    CooperativeTokenAndRemoteAbort,
}

impl BackendCancellationCapability {
    #[must_use]
    pub const fn supports_cooperative_token(self) -> bool {
        matches!(
            self,
            Self::CooperativeToken | Self::CooperativeTokenAndRemoteAbort
        )
    }

    #[must_use]
    pub const fn supports_remote_abort(self) -> bool {
        matches!(
            self,
            Self::RemoteAbort | Self::CooperativeTokenAndRemoteAbort
        )
    }
}

/// How a backend maintains state across root turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendContinuationCapability {
    None,
    InProcessState,
    RemoteState,
}

/// Which interrupted states can be represented without flattening them to errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendWaitCapability {
    None,
    Input,
    Auth,
    InputAndAuth,
}

/// What transcript contract the backend consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendTranscriptCapability {
    FullTranscript,
    IncrementalUserMessagesWithRemoteState,
    SinglePrompt,
}

/// What output shape the backend preserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendOutputCapability {
    Text,
    TextAndArtifacts,
}

/// Optional execution capabilities exposed by a backend implementation.
///
/// Marked `#[non_exhaustive]` so adding a capability bit does not become a
/// source-breaking change for downstream backends. Construct via
/// [`Self::full`] or [`Self::remote_stateless_text`] then mutate fields,
/// rather than struct-literal syntax.
///
/// Backend implementors migrating to delegate state seeding should set
/// [`Self::delegate_state_seed`] to `true` only if `execute_delegate` actually
/// applies `BackendDelegateRunRequest.state_seed` before running the child.
/// Unsupported seeded delegate requests are rejected during validation rather
/// than silently dropping the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendCapabilities {
    pub cancellation: BackendCancellationCapability,
    pub decisions: bool,
    pub overrides: bool,
    pub frontend_tools: bool,
    pub continuation: BackendContinuationCapability,
    pub waits: BackendWaitCapability,
    pub transcript: BackendTranscriptCapability,
    pub output: BackendOutputCapability,
    /// Whether the backend actually applies
    /// `BackendDelegateRunRequest.state_seed` before running the child.
    ///
    /// Default-`false`, opt-in: a backend MUST only set this `true` if its
    /// `execute_delegate` implementation reads the seed and seeds the
    /// child store (currently only the in-process Local backend does so).
    /// Both [`Self::full`] and [`Self::remote_stateless_text`] leave it
    /// `false`; opt in explicitly when you implement seed support.
    pub delegate_state_seed: bool,
}

impl BackendCapabilities {
    /// Convenience constructor for backends that implement every transport
    /// surface (cancellation, waits, frontend tools, full transcript,
    /// continuation, etc.). **Does not** advertise
    /// [`delegate_state_seed`](Self::delegate_state_seed) — that bit is
    /// opt-in and must be set by backends that actually apply the seed.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            cancellation: BackendCancellationCapability::CooperativeToken,
            decisions: true,
            overrides: true,
            frontend_tools: true,
            continuation: BackendContinuationCapability::InProcessState,
            waits: BackendWaitCapability::InputAndAuth,
            transcript: BackendTranscriptCapability::FullTranscript,
            output: BackendOutputCapability::TextAndArtifacts,
            delegate_state_seed: false,
        }
    }

    #[must_use]
    pub const fn remote_stateless_text() -> Self {
        Self {
            cancellation: BackendCancellationCapability::None,
            decisions: false,
            overrides: false,
            frontend_tools: false,
            continuation: BackendContinuationCapability::None,
            waits: BackendWaitCapability::None,
            transcript: BackendTranscriptCapability::SinglePrompt,
            output: BackendOutputCapability::Text,
            delegate_state_seed: false,
        }
    }

    #[must_use]
    pub fn unsupported_root_features(
        &self,
        request: &BackendRootRunRequest<'_>,
    ) -> Vec<&'static str> {
        let mut unsupported = Vec::new();
        if (!request.decisions.is_empty() || request.control.decision_rx.is_some())
            && !self.decisions
        {
            unsupported.push("decisions");
        }
        if request.overrides.is_some() && !self.overrides {
            unsupported.push("overrides");
        }
        if !request.frontend_tools.is_empty() && !self.frontend_tools {
            unsupported.push("frontend_tools");
        }
        if request.is_continuation && self.continuation == BackendContinuationCapability::None {
            unsupported.push("continuation");
        }
        unsupported
    }

    #[must_use]
    pub fn unsupported_delegate_features(
        &self,
        request: &BackendDelegateRunRequest<'_>,
    ) -> Vec<&'static str> {
        let mut unsupported = Vec::new();
        if request.policy.persistence != BackendDelegatePersistence::Ephemeral {
            unsupported.push("delegate_persistence");
        }
        if request.policy.continuation != BackendDelegateContinuation::Disabled
            && self.continuation == BackendContinuationCapability::None
        {
            unsupported.push("continuation");
        }
        if request.state_seed.is_some() && !self.delegate_state_seed {
            unsupported.push("delegate_state_seed");
        }
        unsupported
    }
}

impl Default for BackendCapabilities {
    fn default() -> Self {
        Self::full()
    }
}

/// Helper used by validation and individual backends to reject delegate
/// requests that exercise features the backend does not advertise.
pub fn reject_unsupported_delegate(
    capabilities: &BackendCapabilities,
    request: &BackendDelegateRunRequest<'_>,
) -> Result<(), ExecutionBackendError> {
    let unsupported = capabilities.unsupported_delegate_features(request);
    if !unsupported.is_empty() {
        return Err(ExecutionBackendError::ExecutionFailed(format!(
            "agent '{}' backend does not support: {}",
            request.agent_id,
            unsupported.join(", ")
        )));
    }
    Ok(())
}
