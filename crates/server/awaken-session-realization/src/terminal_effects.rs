// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The terminal effects component and its process-local teardown projection; durable phase
// @formal reason: authority stays in the Session aggregate and every effect keeps its existing fence.
//! Terminal effects of the realization executor: the terminal cleanup effects
//! a Session's terminal assignment authorizes. It admits a cleanup lease
//! against the Session's realization authority, derives the effect fence and
//! the typed Sandbox disposal authorization, harvests the Session's Artifacts,
//! and reads the retained terminal owner and Memory reconciliation plan the
//! cleanup executes. (The caller role it serves is `TerminalCleanup`.)

use crate::session_environment::PreparedBoundEnvironment;

mod cleanup;
mod peer;
mod preparation;
pub mod repository_publication;

/// One process-local teardown projection derived from the frozen aggregate
/// Environment state. It carries no authority of its own; keeping the state and
/// optional physical owner together prevents terminal checkpoint and Sandbox
/// cleanup from re-reading or independently interpreting the slot.
pub(crate) struct TerminalEnvironmentPreparation {
    state: awaken_session_state_contract::SessionEnvironmentState,
    environment: PreparedBoundEnvironment,
    pending_checkpoint: Option<awaken_session_state_contract::SandboxCheckpointRequest>,
}

/// Pure terminal Memory projection produced before Artifact, checkpoint, or
/// provider effects. The contract owns the optional-handle join and intent
/// construction; this transient plan only carries its exact result forward.
pub(crate) struct TerminalMemoryPlan {
    intents: Vec<awaken_session_state_contract::SessionTerminalMemoryIntent>,
    acknowledged_materializations:
        Option<Vec<awaken_provisioning_contract::MemoryMaterializationEvidence>>,
    workspace_id: Option<String>,
}

impl TerminalEnvironmentPreparation {
    /// Select the one artifact edge without conflating aggregate state with
    /// provider I/O safety. Once the provider reports `Disposing`, durable
    /// receipt recovery is mandatory even when the aggregate state would not
    /// authorize a live output read.
    fn artifact_capture_mode(&self) -> Option<crate::artifact_harvest::ArtifactCaptureMode> {
        if !self.environment.permits_live_io() {
            Some(crate::artifact_harvest::ArtifactCaptureMode::ReceiptOnly)
        } else if self.state.binding().is_some() {
            Some(crate::artifact_harvest::ArtifactCaptureMode::Live)
        } else {
            None
        }
    }
}

/// Executes terminal effects over its own state in the installed Sessions.
/// Run and cleanup effects share the one selected Artifact publication operation.
#[derive(Clone)]
pub struct TerminalEffects {
    artifacts: std::sync::Arc<crate::artifact_harvest::ArtifactHarvester>,
    sessions: crate::session_realizations::SessionRealizations,
}

impl TerminalEffects {
    pub fn new(
        sessions: crate::session_realizations::SessionRealizations,
        artifacts: std::sync::Arc<crate::artifact_harvest::ArtifactHarvester>,
    ) -> Self {
        Self {
            artifacts,
            sessions,
        }
    }

    /// The Artifact publication Run and cleanup effects share.
    pub fn artifacts(&self) -> &std::sync::Arc<crate::artifact_harvest::ArtifactHarvester> {
        &self.artifacts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_environment::TerminalEffectMode;

    #[test]
    fn terminal_effect_mode_keeps_live_recovery_and_provider_preparation_coupled() {
        /* Process-local mode table HT0. Causes: C1 provider observation is
         * Ready/filesystem-Terminal, Container-Terminal, primary absence, or Disposing; C2 the
         * exact cleanup owner may be present/absent. Effects: E1 only live mode
         * permits source reads; E2 live and unprepared-recovery require the
         * provider preparation edge; E3 only Disposing proves that source
         * effects were already prepared. The owner presence does not change
         * these rules. This single projector prevents Artifact, Memory, and
         * provider callers from inventing separate observation policies.
         *
         * | Rule | observation projection | live I/O | provider prep | prior prep |
         * | T0 | LiveUnprepared | yes | yes | no |
         * | T1 | RecoverOnlyUnprepared | no | yes | no |
         * | T2 | RecoverOnlyAlreadyPrepared | no | no | yes | */
        let cases = [
            (
                PreparedBoundEnvironment::new(None, None, TerminalEffectMode::LiveUnprepared),
                true,
                true,
                false,
            ),
            (
                PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::RecoverOnlyUnprepared,
                ),
                false,
                true,
                false,
            ),
            (
                PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::RecoverOnlyAlreadyPrepared,
                ),
                false,
                false,
                true,
            ),
        ];
        assert_eq!(
            TerminalEffectMode::for_terminal_source(true),
            TerminalEffectMode::LiveUnprepared,
            "HT0 filesystem data remains readable"
        );
        assert_eq!(
            TerminalEffectMode::for_terminal_source(false),
            TerminalEffectMode::RecoverOnlyUnprepared,
            "HT0 Container terminal source requires receipt-only reads and still needs preparation"
        );
        for (prepared, live_io, provider_preparation, already_prepared) in cases {
            assert_eq!(prepared.permits_live_io(), live_io);
            assert_eq!(
                prepared.requires_provider_preparation(),
                provider_preparation,
            );
            assert_eq!(
                prepared.source_effects_are_already_prepared(),
                already_prepared,
            );
        }
    }

    #[test]
    fn only_agent_authored_terminal_states_read_live_outputs() {
        /* Host terminal-output table HT1. Causes: C1 aggregate state is
         * Resident/Suspending/otherwise; C2 provider I/O mode is Live or
         * RecoverOnly. Effects: E1 Agent-authored+Live uses the one live
         * ArtifactHarvester edge; E2 non-Agent-authored+Live skips live
         * capture; E3 every RecoverOnly state invokes receipt recovery and
         * performs no live read. Rules: HT1a Resident|Suspending+Live=>E1;
         * HT1b otherwise+Live=>E2; HT1c any+RecoverOnly=>E3. C2 dominates C1:
         * a durable Disposing gate must never be mistaken for "no artifacts"
         * merely because the aggregate state is non-Agent-authored. */
        let resident = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "binding-a".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        // The aggregate's binding projection already owns exactly the
        // Resident/Suspending source partition. Exercise the consuming artifact
        // edge rather than maintaining a second variant classifier in Host.
        let live_capture = |state| {
            TerminalEnvironmentPreparation {
                state,
                environment: PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::LiveUnprepared,
                ),
                pending_checkpoint: None,
            }
            .artifact_capture_mode()
        };
        assert_eq!(
            live_capture(resident),
            Some(crate::artifact_harvest::ArtifactCaptureMode::Live),
            "HT1a"
        );
        assert_eq!(
            live_capture(awaken_session_state_contract::SessionEnvironmentState::Unmaterialized),
            None,
            "HT1b"
        );

        let disposing_non_agent_state = TerminalEnvironmentPreparation {
            state: awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
            environment: PreparedBoundEnvironment::new(
                None,
                None,
                TerminalEffectMode::RecoverOnlyAlreadyPrepared,
            ),
            pending_checkpoint: None,
        };
        assert_eq!(
            disposing_non_agent_state.artifact_capture_mode(),
            Some(crate::artifact_harvest::ArtifactCaptureMode::ReceiptOnly),
            "HT1c"
        );
    }
}
