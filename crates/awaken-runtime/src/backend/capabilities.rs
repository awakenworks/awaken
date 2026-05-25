//! Backend capability metadata.

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
