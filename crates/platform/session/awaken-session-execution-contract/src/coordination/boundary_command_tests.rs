use super::*;
use awaken_agent_contract::agent::run::EndCause;

#[test]
fn claimed_boundary_command_preserves_affinity_and_failure_precedence() {
    // Cause/effect graph: C1 epoch absent/zero/nonzero/MAX; C2 the caller's
    // trusted Session affinity absent/present; C3 claimed Run exact/foreign;
    // C4 committed state Running/Awaiting/Ended; C5 cancellation false/true.
    // R1 !C1(epoch absent) -> ignored, even with other invalid inputs.
    // R2 epoch present + !C2 -> original missing-affinity refusal first.
    // R3 C2 + (zero epoch | foreign claim | Running) -> original refusal.
    // R4 otherwise -> exact Session, logical Thread, Run, Agent, epoch and C5.
    // All 96 finite cause combinations run against the real constructor.
    // The source strings are distinct so confusing physical Session affinity
    // with logical Thread or current Agent identity cannot pass this table.
    let session = ThreadId("physical-session".into());
    let thread = ThreadId("logical-child".into());
    let run = RunId("original-run".into());
    let foreign = RunId("foreign-run".into());
    for epoch in [None, Some(0), Some(1), Some(u64::MAX)] {
        for affinity in [None, Some(&session)] {
            for exact in [false, true] {
                for (state, boundary) in [
                    (RunState::Running, false),
                    (RunState::Awaiting, true),
                    (RunState::Ended(EndCause::NaturalEnd), true),
                ] {
                    for cancellation in [false, true] {
                        let command = SessionAgentBoundaryCommand::from_claimed_boundary(
                            epoch,
                            affinity,
                            (&thread, &run, "frozen-agent"),
                            if exact { &run } else { &foreign },
                            &state,
                            cancellation,
                        );
                        if epoch.is_none() {
                            assert_eq!(command.unwrap(), None, "R1");
                        } else if affinity.is_none() {
                            let error = command.unwrap_err();
                            assert_eq!(error.kind, crate::RunErrorKind::BadRequest, "R2");
                            assert_eq!(
                                error.message,
                                "coordinated child settlement has no parent Session affinity",
                                "R2"
                            );
                        } else if epoch == Some(0) || !exact || !boundary {
                            let error = command.unwrap_err();
                            assert_eq!(error.kind, crate::RunErrorKind::BadRequest, "R3");
                            assert_eq!(
                                error.message,
                                "coordinated child settlement coordinates are inconsistent",
                                "R3"
                            );
                        } else {
                            let command = command.unwrap().unwrap();
                            assert_eq!(command.session_id, session.0, "R4 physical owner");
                            assert_eq!(command.source_thread_id, thread, "R4 logical Thread");
                            assert_eq!(command.source_run_id, run, "R4 exact Run");
                            assert_eq!(
                                command.source_agent_id, "frozen-agent",
                                "R4 original Agent"
                            );
                            assert_eq!(Some(command.session_activity_epoch), epoch, "R4 epoch");
                            assert_eq!(
                                command.cancellation_requested, cancellation,
                                "R4 cancellation"
                            );
                        }
                    }
                }
            }
        }
    }
}
