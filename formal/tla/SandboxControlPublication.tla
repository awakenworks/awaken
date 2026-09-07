-------------------- MODULE SandboxControlPublication --------------------
EXTENDS Naturals, TLC

CONSTANT MaxLeases

Phases == {"Vacant", "Active", "Closed"}
Leases == 1..MaxLeases

VARIABLES phase, nextLease, activeLease, staleLease, closedOnce,
          task, cancelled, retained, closers

Callers == 1..2
Quiescent == {"Absent", "Joined"}
vars == <<phase, nextLease, activeLease, staleLease, closedOnce,
          task, cancelled, retained, closers>>

\* Cause/effect design:
\* - reserving a vacant slot publishes one fresh owner identity;
\* - releasing the current identity vacates the slot, while releasing a stale
\*   identity has no effect on a newer owner;
\* - closing from any live phase is absorbing;
\* - exhausting the finite model stops allocation instead of modelling wrap.
\* - setup and serving belong to the same installed task, before any async I/O;
\* - Drop requests cancellation but cannot release until join;
\* - two disposers retain the same task across caller cancellation/retry;
\* - only join followed by synchronous resource cleanup permits a return.
\* Task exit is an environmental action, NOT implied by cancellation. Liveness
\* assumes eventual task exit and a fair retrying disposer. This bounded model
\* does not prove Rust/Tokio refinement, socket identity or process-tree death.

Init ==
    /\ phase = "Vacant"
    /\ nextLease = 1
    /\ activeLease = 0
    /\ staleLease = 0
    /\ closedOnce = FALSE
    /\ task = "Absent"
    /\ cancelled = FALSE
    /\ retained = FALSE
    /\ closers = [i \in Callers |-> "Idle"]

Reserve ==
    /\ phase = "Vacant"
    /\ nextLease \in Leases
    /\ phase' = "Active"
    /\ activeLease' = nextLease
    /\ nextLease' = nextLease + 1
    /\ task' = "Absent"
    /\ cancelled' = FALSE
    /\ UNCHANGED <<staleLease, closedOnce, retained, closers>>

StartTask ==
    /\ phase = "Active" /\ ~cancelled /\ task = "Absent"
    /\ task' = "Running"
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   cancelled, retained, closers>>

DropPublication ==
    /\ phase = "Active" /\ ~cancelled
    /\ cancelled' = TRUE
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   task, retained, closers>>

TaskExit ==
    /\ task = "Running"
    /\ task' = "Exited"
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   cancelled, retained, closers>>

JoinTask ==
    /\ task = "Exited"
    /\ task' = "Joined"
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   cancelled, retained, closers>>

ReleaseCurrent ==
    /\ phase = "Active"
    /\ task \in Quiescent
    /\ phase' = "Vacant"
    /\ staleLease' = activeLease
    /\ activeLease' = 0
    /\ UNCHANGED <<nextLease, closedOnce, task, cancelled, retained, closers>>

ReleaseStale ==
    /\ phase = "Active"
    /\ staleLease \in Leases
    /\ staleLease # activeLease
    /\ UNCHANGED vars

Close(i) ==
    /\ phase # "Closed"
    /\ phase' = "Closed"
    /\ activeLease' = 0
    /\ staleLease' = IF phase = "Active" THEN activeLease ELSE staleLease
    /\ closedOnce' = TRUE
    /\ retained' = (phase = "Active")
    /\ cancelled' = TRUE
    /\ closers' = [closers EXCEPT ![i] = "Waiting"]
    /\ UNCHANGED <<nextLease, task>>

RepeatClose(i) ==
    /\ phase = "Closed"
    /\ closers[i] \in {"Idle", "Cancelled"}
    /\ closers' = [closers EXCEPT ![i] = "Waiting"]
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   task, cancelled, retained>>

CancelCloser(i) ==
    /\ closers[i] = "Waiting"
    /\ closers' = [closers EXCEPT ![i] = "Cancelled"]
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   task, cancelled, retained>>

FinishClose(i) ==
    /\ closers[i] = "Waiting"
    /\ (~retained \/ task \in Quiescent)
    /\ closers' = [closers EXCEPT ![i] = "Done"]
    /\ retained' = FALSE
    /\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,
                   task, cancelled>>

Next == Reserve \/ StartTask \/ DropPublication \/ TaskExit \/ JoinTask
        \/ ReleaseCurrent \/ ReleaseStale
        \/ (\E i \in Callers: Close(i) \/ RepeatClose(i) \/ CancelCloser(i) \/ FinishClose(i))

Spec == Init /\ [][Next]_vars
        /\ WF_vars(TaskExit) /\ WF_vars(JoinTask)
        /\ WF_vars(RepeatClose(1)) /\ SF_vars(FinishClose(1))

TypeOK ==
    /\ phase \in Phases
    /\ nextLease \in 1..(MaxLeases + 1)
    /\ activeLease \in 0..MaxLeases
    /\ staleLease \in 0..MaxLeases
    /\ closedOnce \in BOOLEAN
    /\ task \in {"Absent", "Running", "Exited", "Joined"}
    /\ cancelled \in BOOLEAN
    /\ retained \in BOOLEAN
    /\ closers \in [Callers -> {"Idle", "Waiting", "Cancelled", "Done"}]

ExactlyOneOwnerWhileActive ==
    (phase = "Active") <=> (activeLease \in Leases)

FreshIdentityNeverReusesStaleLease ==
    /\ (phase = "Active" => activeLease < nextLease)
    /\ (staleLease # 0 => activeLease # staleLease)

CloseIsAbsorbing ==
    closedOnce <=> (phase = "Closed")

ExhaustionCannotWrap ==
    nextLease = MaxLeases + 1 => activeLease < nextLease

CleanupParticipantRetained ==
    phase = "Closed" /\ task \notin Quiescent => retained

ReturnRequiresJoin ==
    (\E i \in Callers: closers[i] = "Done") => task \in Quiescent /\ ~retained

ReleaseRequiresJoin ==
    phase = "Vacant" => task \in Quiescent

NoStartAfterCancellation ==
    [][task # "Running" /\ task' = "Running" => ~cancelled /\ phase # "Closed"]_vars

JoinRequiresExit ==
    [][task # "Joined" /\ task' = "Joined" => task = "Exited"]_vars

DisposalEventuallyReturns ==
    (phase = "Closed") ~> (closers[1] = "Done")

NoConcurrentWaitReached == ~(closers[1] = "Waiting" /\ closers[2] = "Waiting" /\ task = "Running")
NoCancelledPendingReached == ~(closers[1] = "Cancelled" /\ retained /\ task = "Running")
NoFinishedDisposalReached == ~(phase = "Closed" /\ closers[1] = "Done" /\ task = "Joined")
NoDroppedPendingReached == ~(phase = "Active" /\ cancelled /\ task = "Running")

=============================================================================
