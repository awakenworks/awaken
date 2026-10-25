// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: The one lowering of a Session application Run reservation into a durable, unclaimable
// @formal reason: Session dispatch row: it freezes the command before Skill expansion, may only narrow the
// @formal reason: Session's tool authority and maps every store outcome once. Classification, not a
// @formal reason: formal-proof claim.
//! Reserving one Session Run: the Session application's admitted command,
//! lowered over the Session's frozen inputs into one durable dispatch row a
//! Worker cannot claim until the Session commits the Run's activity.
//!
//! Every process that reserves a Session Run (the Coordinator over its
//! dispatch projections, a co-located executor over its installed Sessions)
//! supplies the frozen inputs and its dispatch preparation; the lowering, the
//! placement it requires and the outcome it reports are decided here once.

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::{
    PlacementRequirements, SessionRunReservationOutcome, SessionRunReservations,
};
use awaken_dispatch_execution::session_dispatch::DispatchPreparation;
use awaken_ext_skills::SkillRegistry;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
use awaken_session_execution_contract::{AdmitSessionRun, SessionRunReservation};
use awaken_session_types::RunError;

/// How long a reservation stays exclusive to the Session that reserved it
/// before its activity must commit.
pub const SESSION_RUN_RESERVATION_TTL_MS: u64 = 30_000;

/// Refuse a reservation command that misses an identity or its input.
pub fn validate_session_run_command(command: &AdmitSessionRun) -> Result<(), RunError> {
    if command.session_id.as_str().trim().is_empty()
        || command.agent_id.trim().is_empty()
        || command.operation_id.trim().is_empty()
        || command.run_id.0.trim().is_empty()
        || command.messages.is_empty()
    {
        return Err(RunError::bad_request(
            "Session Run reservation is incomplete",
        ));
    }
    Ok(())
}

/// The Session's frozen inputs one reservation lowers.
pub struct SessionRunInputs<'a> {
    /// The Session's effective Agent snapshot the Run executes.
    pub snapshot: &'a ExecutableAgentSnapshot,
    /// The Session's frozen Skill registry, expanding a leading `/skill`.
    pub skills: Option<&'a dyn SkillRegistry>,
    /// The Session's frozen model selection.
    pub model_ref_override: Option<String>,
}

/// Reserve `command`'s Run over the Session's frozen `inputs`.
///
/// Decision table (causes → effect):
/// | Rule | cause | effect |
/// |---|---|---|
/// | V1 | an identity or the input is missing | refused before any read (`validate_session_run_command`) |
/// | R1 | a leading `/skill` names a user-invocable frozen Skill | the input carries its instructions |
/// | R2 | the requirements widen the Session's tool authority | refused |
/// | R3 | the requirements name Worker capabilities | the row requires a registered Worker with them |
/// | R4 | the store reserves, replays, recovers, completed or activated the Run | that outcome |
/// | R5 | the Run id was reserved with another input | refused as a reused id |
/// | R6 | the store fails | retryable |
///
/// The command's fingerprint is taken before Skill expansion, so a retry
/// that reconstructs the payload from changed projections cannot reuse it.
pub async fn reserve_session_run<'p, R>(
    command: AdmitSessionRun,
    inputs: SessionRunInputs<'_>,
    prepare: impl FnOnce(RunActivation) -> DispatchPreparation<'p>,
    reservations: &R,
) -> Result<SessionRunReservation, RunError>
where
    R: SessionRunReservations + ?Sized,
{
    let command_fingerprint = command.command_fingerprint();
    let session = command.session_id.as_str();
    let input = match inputs.skills {
        Some(registry) => {
            awaken_ext_skills::expand_slash_commands(registry, session, command.messages)
                .map_err(RunError::internal)?
        }
        None => command.messages,
    };
    let mut activation = RunActivation::new(
        command.run_id,
        ThreadId(session.to_owned()),
        inputs.snapshot.clone(),
        input,
    )
    // Application requirements may only narrow the Session-owned tool
    // authority; they cannot restore a capability the frozen Session profile
    // removed.
    .narrow_tools(command.execution_requirements.tool_capability_narrowing)
    .map_err(RunError::bad_request)?;
    activation.model_ref_override = inputs.model_ref_override;
    activation.data_subject_id = command
        .data_subject_id
        .map(awaken_runtime_contract::DataSubjectId);
    let mut request = prepare(activation)
        .with_traceparent(command.traceparent)
        .map_err(RunError::from)?
        .with_session_command_fingerprint(command_fingerprint)
        .with_session_run_replacement(command.replacement);
    require_registered_worker(
        &mut request.placement,
        command.execution_requirements.required_worker_capabilities,
    );
    let outcome = reservations
        .reserve_session_run(request, SESSION_RUN_RESERVATION_TTL_MS)
        .await
        .map_err(|error| RunError::unavailable(error.to_string()))?;
    session_run_reservation(outcome)
}

/// Protocol-specific capabilities are implemented only by a registered
/// Worker: a Run that requires any never falls back to a generic local runner.
fn require_registered_worker(
    placement: &mut PlacementRequirements,
    capabilities: std::collections::BTreeSet<String>,
) {
    if capabilities.is_empty() {
        return;
    }
    let strict_remote = PlacementRequirements::remote_required();
    placement.location = strict_remote.location;
    if placement.contract_version == 0 {
        placement.contract_version = strict_remote.contract_version;
        placement.dispatch_contract_version = strict_remote.dispatch_contract_version;
        placement.runtime_protocol_version = strict_remote.runtime_protocol_version;
    }
    placement.required_capabilities.extend(capabilities);
}

/// The Session application's view of the store's reservation outcome.
fn session_run_reservation(
    outcome: SessionRunReservationOutcome,
) -> Result<SessionRunReservation, RunError> {
    match outcome {
        SessionRunReservationOutcome::Reserved => Ok(SessionRunReservation::Reserved),
        SessionRunReservationOutcome::AlreadyReserved => Ok(SessionRunReservation::AlreadyReserved),
        SessionRunReservationOutcome::RecoveryClaimed => Ok(SessionRunReservation::RecoveryClaimed),
        SessionRunReservationOutcome::AlreadyActivated {
            session_activity_epoch,
        } => Ok(SessionRunReservation::AlreadyActivated {
            session_activity_epoch,
        }),
        SessionRunReservationOutcome::Completed => Ok(SessionRunReservation::Completed),
        SessionRunReservationOutcome::Conflict => Err(RunError::bad_request(
            "Session Run id was reused with different input",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_outcomes_and_placement_follow_their_decision_table() {
        // R4: every store outcome but a conflict is the application's outcome.
        let outcomes = [
            (
                SessionRunReservationOutcome::Reserved,
                SessionRunReservation::Reserved,
            ),
            (
                SessionRunReservationOutcome::AlreadyReserved,
                SessionRunReservation::AlreadyReserved,
            ),
            (
                SessionRunReservationOutcome::RecoveryClaimed,
                SessionRunReservation::RecoveryClaimed,
            ),
            (
                SessionRunReservationOutcome::AlreadyActivated {
                    session_activity_epoch: 7,
                },
                SessionRunReservation::AlreadyActivated {
                    session_activity_epoch: 7,
                },
            ),
            (
                SessionRunReservationOutcome::Completed,
                SessionRunReservation::Completed,
            ),
        ];
        for (store, application) in outcomes {
            assert_eq!(
                session_run_reservation(store).expect("R4"),
                application,
                "R4"
            );
        }
        // R5: a reused id is the caller's error.
        let reused =
            session_run_reservation(SessionRunReservationOutcome::Conflict).expect_err("R5");
        assert_eq!(reused.code, "invalid_request", "R5");

        // R3: required capabilities force a registered Worker and keep an
        // explicit contract version.
        let mut local = PlacementRequirements::default();
        require_registered_worker(&mut local, Default::default());
        assert_eq!(local, PlacementRequirements::default(), "R3 no capability");
        require_registered_worker(&mut local, ["acp".to_string()].into());
        let strict = PlacementRequirements::remote_required();
        assert_eq!(local.location, strict.location, "R3 registered Worker");
        assert_eq!(
            local.contract_version, strict.contract_version,
            "R3 contract"
        );
        assert_eq!(
            local.required_capabilities,
            ["acp".to_string()].into(),
            "R3"
        );
        let mut pinned = PlacementRequirements {
            contract_version: strict.contract_version + 1,
            ..PlacementRequirements::default()
        };
        require_registered_worker(&mut pinned, ["acp".to_string()].into());
        assert_eq!(
            pinned.contract_version,
            strict.contract_version + 1,
            "R3 explicit contract kept"
        );
    }
}
