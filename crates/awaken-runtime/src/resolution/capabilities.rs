use crate::backend::{
    BackendCancellationCapability, BackendContinuationCapability, BackendOutputCapability,
    BackendTranscriptCapability, BackendWaitCapability,
};

use super::{PersistenceRequirement, RunFeatureSet};

pub type CancellationCapability = BackendCancellationCapability;
pub type ContinuationCapability = BackendContinuationCapability;
pub type WaitCapability = BackendWaitCapability;
pub type TranscriptCapability = BackendTranscriptCapability;
pub type OutputCapability = BackendOutputCapability;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionCapability {
    None,
    LiveOnly,
    DurableResume,
    LiveAndDurable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideCapability {
    None,
    InferenceParams,
    ModelAndParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendToolCapability {
    None,
    DescriptorsOnly,
    Executable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceCapability {
    Ephemeral,
    Checkpoint,
    CrossSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendProfile {
    pub cancellation: CancellationCapability,
    pub continuation: ContinuationCapability,
    pub decisions: DecisionCapability,
    pub overrides: OverrideCapability,
    pub frontend_tools: FrontendToolCapability,
    pub persistence: PersistenceCapability,
    pub waits: WaitCapability,
    pub transcript: TranscriptCapability,
    pub output: OutputCapability,
}

impl BackendProfile {
    #[must_use]
    pub const fn full_local() -> Self {
        Self {
            cancellation: CancellationCapability::CooperativeToken,
            continuation: ContinuationCapability::InProcessState,
            decisions: DecisionCapability::LiveAndDurable,
            overrides: OverrideCapability::ModelAndParams,
            frontend_tools: FrontendToolCapability::Executable,
            persistence: PersistenceCapability::Checkpoint,
            waits: WaitCapability::InputAndAuth,
            transcript: TranscriptCapability::FullTranscript,
            output: OutputCapability::TextAndArtifacts,
        }
    }

    #[must_use]
    pub const fn remote_stateless_text() -> Self {
        Self {
            cancellation: CancellationCapability::None,
            continuation: ContinuationCapability::None,
            decisions: DecisionCapability::None,
            overrides: OverrideCapability::None,
            frontend_tools: FrontendToolCapability::None,
            persistence: PersistenceCapability::Ephemeral,
            waits: WaitCapability::None,
            transcript: TranscriptCapability::SinglePrompt,
            output: OutputCapability::Text,
        }
    }

    #[must_use]
    pub fn check(&self, req: &BackendRequirements) -> CapabilityDecision {
        let mut mismatches = Vec::new();
        macro_rules! check_cap {
            ($field:ident, $supports:expr) => {
                if let Some(required) = req.$field
                    && !$supports(self.$field, required)
                {
                    mismatches.push(CapabilityMismatch {
                        capability: stringify!($field),
                        required: format!("{:?}", required),
                        actual: format!("{:?}", self.$field),
                    });
                }
            };
        }
        check_cap!(cancellation, supports_cancellation);
        check_cap!(continuation, supports_continuation);
        check_cap!(decisions, supports_decision);
        check_cap!(overrides, supports_override);
        check_cap!(frontend_tools, supports_frontend_tool);
        check_cap!(persistence, supports_persistence);
        check_cap!(waits, supports_wait);
        check_cap!(transcript, supports_transcript);
        check_cap!(output, supports_output);
        if mismatches.is_empty() {
            CapabilityDecision::Supported
        } else {
            CapabilityDecision::Unsupported(mismatches)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRequirements {
    pub cancellation: Option<CancellationCapability>,
    pub continuation: Option<ContinuationCapability>,
    pub decisions: Option<DecisionCapability>,
    pub overrides: Option<OverrideCapability>,
    pub frontend_tools: Option<FrontendToolCapability>,
    pub persistence: Option<PersistenceCapability>,
    pub waits: Option<WaitCapability>,
    pub transcript: Option<TranscriptCapability>,
    pub output: Option<OutputCapability>,
}

impl BackendRequirements {
    #[must_use]
    pub fn from_features(features: &RunFeatureSet) -> Self {
        Self {
            cancellation: None,
            continuation: features
                .is_continuation
                .then_some(ContinuationCapability::InProcessState),
            decisions: decision_requirement(features),
            overrides: features
                .has_overrides
                .then_some(OverrideCapability::InferenceParams),
            frontend_tools: features
                .has_frontend_tools
                .then_some(FrontendToolCapability::DescriptorsOnly),
            persistence: (features.requested_persistence
                == PersistenceRequirement::CheckpointRequired)
                .then_some(PersistenceCapability::Checkpoint),
            waits: (features.has_seeded_decisions || features.has_live_decision_channel)
                .then_some(WaitCapability::InputAndAuth),
            transcript: Some(TranscriptCapability::FullTranscript),
            output: Some(OutputCapability::Text),
        }
    }
}

fn decision_requirement(features: &RunFeatureSet) -> Option<DecisionCapability> {
    match (
        features.has_seeded_decisions,
        features.has_live_decision_channel,
    ) {
        (false, false) => None,
        (false, true) => Some(DecisionCapability::LiveOnly),
        (true, false) => Some(DecisionCapability::DurableResume),
        (true, true) => Some(DecisionCapability::LiveAndDurable),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityMismatch {
    pub capability: &'static str,
    pub required: String,
    pub actual: String,
}

pub enum CapabilityDecision {
    Supported,
    Unsupported(Vec<CapabilityMismatch>),
}

fn supports_cancellation(actual: CancellationCapability, required: CancellationCapability) -> bool {
    matches!(
        (actual, required),
        (_, CancellationCapability::None)
            | (
                CancellationCapability::CooperativeToken,
                CancellationCapability::CooperativeToken
            )
            | (
                CancellationCapability::RemoteAbort,
                CancellationCapability::RemoteAbort
            )
            | (
                CancellationCapability::CooperativeTokenAndRemoteAbort,
                CancellationCapability::CooperativeToken
                    | CancellationCapability::RemoteAbort
                    | CancellationCapability::CooperativeTokenAndRemoteAbort
            )
    )
}

fn supports_continuation(actual: ContinuationCapability, required: ContinuationCapability) -> bool {
    actual == required || matches!(actual, ContinuationCapability::RemoteState)
}

fn supports_decision(actual: DecisionCapability, required: DecisionCapability) -> bool {
    use DecisionCapability::*;
    matches!(
        (actual, required),
        (_, None)
            | (LiveOnly, LiveOnly)
            | (DurableResume, DurableResume)
            | (LiveAndDurable, LiveOnly | DurableResume | LiveAndDurable)
    )
}

fn supports_override(actual: OverrideCapability, required: OverrideCapability) -> bool {
    use OverrideCapability::*;
    matches!(
        (actual, required),
        (_, None)
            | (InferenceParams, InferenceParams)
            | (ModelAndParams, InferenceParams | ModelAndParams)
    )
}

fn supports_frontend_tool(
    actual: FrontendToolCapability,
    required: FrontendToolCapability,
) -> bool {
    use FrontendToolCapability::*;
    matches!(
        (actual, required),
        (_, None) | (DescriptorsOnly, DescriptorsOnly) | (Executable, DescriptorsOnly | Executable)
    )
}

fn supports_persistence(actual: PersistenceCapability, required: PersistenceCapability) -> bool {
    use PersistenceCapability::*;
    matches!(
        (actual, required),
        (_, Ephemeral) | (Checkpoint, Checkpoint) | (CrossSession, Checkpoint | CrossSession)
    )
}

fn supports_wait(actual: WaitCapability, required: WaitCapability) -> bool {
    matches!(
        (actual, required),
        (_, WaitCapability::None)
            | (WaitCapability::Input, WaitCapability::Input)
            | (WaitCapability::Auth, WaitCapability::Auth)
            | (
                WaitCapability::InputAndAuth,
                WaitCapability::Input | WaitCapability::Auth | WaitCapability::InputAndAuth
            )
    )
}

fn supports_transcript(actual: TranscriptCapability, required: TranscriptCapability) -> bool {
    matches!(
        (actual, required),
        (TranscriptCapability::FullTranscript, _)
            | (
                TranscriptCapability::IncrementalUserMessagesWithRemoteState,
                TranscriptCapability::IncrementalUserMessagesWithRemoteState
                    | TranscriptCapability::FullTranscript
                    | TranscriptCapability::SinglePrompt
            )
            | (
                TranscriptCapability::SinglePrompt,
                TranscriptCapability::SinglePrompt
            )
    )
}

fn supports_output(actual: OutputCapability, required: OutputCapability) -> bool {
    matches!(
        (actual, required),
        (
            OutputCapability::TextAndArtifacts,
            OutputCapability::Text | OutputCapability::TextAndArtifacts
        ) | (OutputCapability::Text, OutputCapability::Text)
    )
}
