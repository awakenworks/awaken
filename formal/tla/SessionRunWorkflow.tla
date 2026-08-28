------------------------- MODULE SessionRunWorkflow -------------------------
EXTENDS Naturals

\* One composed model of the product boundary: Session desired truth, Work and
\* realization leases, durable Run reservation, activity receipt, Worker claim,
\* ToolReply consumption, terminal commit, and settlement. Work/realization
\* leases authorize physical effects only; Dispatch owns Run admission/attempts.
CONSTANTS AgentRevisions, EnvironmentRevisions, CredentialRevisions,
          Workers, NoRevision, NoWorker, MaxEpoch

ASSUME
    /\ AgentRevisions # {}
    /\ EnvironmentRevisions # {}
    /\ CredentialRevisions # {}
    /\ Workers # {}
    /\ NoRevision \notin AgentRevisions \cup EnvironmentRevisions \cup CredentialRevisions
    /\ NoWorker \notin Workers
    /\ MaxEpoch \in Nat \ {0}

VARIABLES sessionState, agentRevision, environmentRevision,
          workState, workOwner, workEpoch,
          realizationState, realizationOwner, realizationEpoch,
          dispatchState, dispatchOwner, dispatchEpoch,
          reservationPersisted, activityEpoch,
          runtimeState, credentialRevision, materializedRevision,
          pendingTool, toolReplyCommitted,
          outputCommitted, terminalOwner, terminalEpoch

vars == <<sessionState, agentRevision, environmentRevision,
          workState, workOwner, workEpoch,
          realizationState, realizationOwner, realizationEpoch,
          dispatchState, dispatchOwner, dispatchEpoch,
          reservationPersisted, activityEpoch,
          runtimeState, credentialRevision, materializedRevision,
          pendingTool, toolReplyCommitted,
          outputCommitted, terminalOwner, terminalEpoch>>

Init ==
    /\ sessionState = "Absent"
    /\ agentRevision = NoRevision
    /\ environmentRevision = NoRevision
    /\ workState = "Absent"
    /\ workOwner = NoWorker
    /\ workEpoch = 0
    /\ realizationState = "Absent"
    /\ realizationOwner = NoWorker
    /\ realizationEpoch = 0
    /\ dispatchState = "Absent"
    /\ dispatchOwner = NoWorker
    /\ dispatchEpoch = 0
    /\ reservationPersisted = FALSE
    /\ activityEpoch = 0
    /\ runtimeState = "Absent"
    /\ credentialRevision \in CredentialRevisions
    /\ materializedRevision = NoRevision
    /\ pendingTool = FALSE
    /\ toolReplyCommitted = FALSE
    /\ outputCommitted = FALSE
    /\ terminalOwner = NoWorker
    /\ terminalEpoch = 0

CreateSession(a, e) ==
    /\ sessionState = "Absent"
    /\ a \in AgentRevisions
    /\ e \in EnvironmentRevisions
    /\ sessionState' = "Ready"
    /\ agentRevision' = a
    /\ environmentRevision' = e
    /\ workState' = "Queued"
    /\ UNCHANGED <<workOwner, workEpoch, realizationState, realizationOwner,
                   realizationEpoch, dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

ClaimWork(w) ==
    /\ w \in Workers
    /\ workState = "Queued"
    /\ workEpoch < MaxEpoch
    /\ workState' = "Leased"
    /\ workOwner' = w
    /\ workEpoch' = workEpoch + 1
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

ExpireWork ==
    /\ workState = "Leased"
    /\ runtimeState # "Running"
    /\ workState' = "Queued"
    /\ workOwner' = NoWorker
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

ClaimRealization(w) ==
    /\ w \in Workers
    /\ workState = "Leased"
    /\ workOwner = w
    /\ realizationState \in {"Absent", "Ready"}
    /\ runtimeState # "Running"
    /\ realizationEpoch < MaxEpoch
    /\ realizationState' = "Leased"
    /\ realizationOwner' = w
    /\ realizationEpoch' = realizationEpoch + 1
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

CompleteRealization(w, claimEpoch) ==
    /\ realizationState = "Leased"
    /\ realizationOwner = w
    /\ realizationEpoch = claimEpoch
    /\ realizationState' = "Ready"
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

\* The crash-boundary fact is committed before any Session activity is opened.
ReserveRun ==
    /\ sessionState = "Ready"
    /\ dispatchState = "Absent"
    /\ dispatchState' = "Reserved"
    /\ reservationPersisted' = TRUE
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchOwner, dispatchEpoch, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

OpenActivity ==
    /\ dispatchState = "Reserved"
    /\ reservationPersisted
    /\ activityEpoch' = 1
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, runtimeState, credentialRevision,
                   materializedRevision, pendingTool, toolReplyCommitted,
                   outputCommitted, terminalOwner, terminalEpoch>>

ActivateReservation ==
    /\ dispatchState = "Reserved"
    /\ activityEpoch > 0
    /\ dispatchState' = "Pending"
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchOwner, dispatchEpoch, reservationPersisted,
                   activityEpoch, runtimeState, credentialRevision,
                   materializedRevision, pendingTool, toolReplyCommitted,
                   outputCommitted, terminalOwner, terminalEpoch>>

RecoverReservation(w) ==
    /\ w \in Workers
    /\ dispatchState = "Reserved"
    /\ dispatchEpoch < MaxEpoch
    /\ dispatchState' = "ReservationLeased"
    /\ dispatchOwner' = w
    /\ dispatchEpoch' = dispatchEpoch + 1
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

RecoverAdmission(w, claimEpoch) ==
    /\ dispatchState = "ReservationLeased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ activityEpoch = 0
    /\ activityEpoch' = 1
    /\ dispatchState' = "Pending"
    /\ dispatchOwner' = NoWorker
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchEpoch, reservationPersisted, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

RetryAdmission(w, claimEpoch) ==
    /\ dispatchState = "ReservationLeased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ dispatchState' = "Reserved"
    /\ dispatchOwner' = NoWorker
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchEpoch, reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, materializedRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

ClaimRun(w) ==
    /\ w \in Workers
    /\ dispatchState = "Pending"
    /\ activityEpoch > 0
    /\ dispatchEpoch < MaxEpoch
    /\ dispatchState' = "Leased"
    /\ dispatchOwner' = w
    /\ dispatchEpoch' = dispatchEpoch + 1
    /\ materializedRevision' = NoRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, pendingTool, toolReplyCommitted,
                   outputCommitted, terminalOwner, terminalEpoch>>

Materialize(w, claimEpoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ workState = "Leased"
    /\ workOwner = w
    /\ realizationState = "Ready"
    /\ realizationOwner = w
    /\ materializedRevision' = credentialRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, runtimeState,
                   credentialRevision, pendingTool, toolReplyCommitted,
                   outputCommitted, terminalOwner, terminalEpoch>>

Execute(w, claimEpoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ workState = "Leased"
    /\ workOwner = w
    /\ realizationState = "Ready"
    /\ realizationOwner = w
    /\ materializedRevision = credentialRevision
    /\ runtimeState \in {"Absent", "Running", "Awaiting"}
    /\ runtimeState' = "Running"
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, credentialRevision,
                   materializedRevision, pendingTool, toolReplyCommitted,
                   outputCommitted, terminalOwner, terminalEpoch>>

AwaitTool(w, claimEpoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ runtimeState = "Running"
    /\ ~pendingTool
    /\ ~toolReplyCommitted
    /\ dispatchState' = "Awaiting"
    /\ dispatchOwner' = NoWorker
    /\ runtimeState' = "Awaiting"
    /\ pendingTool' = TRUE
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchEpoch, reservationPersisted, activityEpoch,
                   credentialRevision, materializedRevision, toolReplyCommitted,
                   outputCommitted, terminalOwner, terminalEpoch>>

DeliverToolReply(w) ==
    /\ w \in Workers
    /\ dispatchState = "Awaiting"
    /\ runtimeState = "Awaiting"
    /\ pendingTool
    /\ ~toolReplyCommitted
    /\ dispatchEpoch < MaxEpoch
    /\ dispatchState' = "Leased"
    /\ dispatchOwner' = w
    /\ dispatchEpoch' = dispatchEpoch + 1
    /\ runtimeState' = "Awaiting"
    /\ pendingTool' = FALSE
    /\ toolReplyCommitted' = TRUE
    /\ materializedRevision' = NoRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   reservationPersisted, activityEpoch, credentialRevision,
                   outputCommitted, terminalOwner, terminalEpoch>>

CommitTerminal(w, claimEpoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ runtimeState = "Running"
    /\ runtimeState' = "Ended"
    /\ outputCommitted' = TRUE
    /\ terminalOwner' = w
    /\ terminalEpoch' = claimEpoch
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchState, dispatchOwner, dispatchEpoch,
                   reservationPersisted, activityEpoch, credentialRevision,
                   materializedRevision, pendingTool, toolReplyCommitted>>

Settle(w, claimEpoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = w
    /\ dispatchEpoch = claimEpoch
    /\ runtimeState = "Ended"
    /\ outputCommitted
    /\ dispatchState' = "Removed"
    /\ dispatchOwner' = NoWorker
    /\ runtimeState' = "Settled"
    /\ materializedRevision' = NoRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   workState, workOwner, workEpoch,
                   realizationState, realizationOwner, realizationEpoch,
                   dispatchEpoch, reservationPersisted, activityEpoch,
                   credentialRevision, pendingTool,
                   toolReplyCommitted, outputCommitted, terminalOwner, terminalEpoch>>

CreateSessionAny == \E a \in AgentRevisions, e \in EnvironmentRevisions: CreateSession(a, e)
ClaimWorkAny == \E w \in Workers: ClaimWork(w)
ClaimRealizationAny == \E w \in Workers: ClaimRealization(w)
CompleteRealizationAny == \E w \in Workers, e \in 0..MaxEpoch: CompleteRealization(w, e)
RecoverReservationAny == \E w \in Workers: RecoverReservation(w)
RecoverAdmissionAny == \E w \in Workers, e \in 0..MaxEpoch: RecoverAdmission(w, e)
RetryAdmissionAny == \E w \in Workers, e \in 0..MaxEpoch: RetryAdmission(w, e)
ClaimRunAny == \E w \in Workers: ClaimRun(w)
MaterializeAny == \E w \in Workers, e \in 0..MaxEpoch: Materialize(w, e)
ExecuteAny == \E w \in Workers, e \in 0..MaxEpoch: Execute(w, e)
AwaitToolAny == \E w \in Workers, e \in 0..MaxEpoch: AwaitTool(w, e)
DeliverToolReplyAny == \E w \in Workers: DeliverToolReply(w)
CommitTerminalAny == \E w \in Workers, e \in 0..MaxEpoch: CommitTerminal(w, e)
SettleAny == \E w \in Workers, e \in 0..MaxEpoch: Settle(w, e)

Next ==
    \/ CreateSessionAny
    \/ ClaimWorkAny
    \/ ExpireWork
    \/ ClaimRealizationAny
    \/ CompleteRealizationAny
    \/ ReserveRun
    \/ OpenActivity
    \/ ActivateReservation
    \/ RecoverReservationAny
    \/ RecoverAdmissionAny
    \/ RetryAdmissionAny
    \/ ClaimRunAny
    \/ MaterializeAny
    \/ ExecuteAny
    \/ AwaitToolAny
    \/ DeliverToolReplyAny
    \/ CommitTerminalAny
    \/ SettleAny

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ sessionState \in {"Absent", "Ready"}
    /\ agentRevision \in AgentRevisions \cup {NoRevision}
    /\ environmentRevision \in EnvironmentRevisions \cup {NoRevision}
    /\ workState \in {"Absent", "Queued", "Leased"}
    /\ workOwner \in Workers \cup {NoWorker}
    /\ workEpoch \in 0..MaxEpoch
    /\ realizationState \in {"Absent", "Leased", "Ready"}
    /\ realizationOwner \in Workers \cup {NoWorker}
    /\ realizationEpoch \in 0..MaxEpoch
    /\ dispatchState \in {"Absent", "Reserved", "ReservationLeased", "Pending", "Leased", "Awaiting", "Removed"}
    /\ dispatchOwner \in Workers \cup {NoWorker}
    /\ dispatchEpoch \in 0..MaxEpoch
    /\ reservationPersisted \in BOOLEAN
    /\ activityEpoch \in 0..1
    /\ runtimeState \in {"Absent", "Running", "Awaiting", "Ended", "Settled"}
    /\ credentialRevision \in CredentialRevisions
    /\ materializedRevision \in CredentialRevisions \cup {NoRevision}
    /\ pendingTool \in BOOLEAN
    /\ toolReplyCommitted \in BOOLEAN
    /\ outputCommitted \in BOOLEAN
    /\ terminalOwner \in Workers \cup {NoWorker}
    /\ terminalEpoch \in 0..MaxEpoch

SessionPinsAreImmutable == sessionState = "Ready" =>
    agentRevision \in AgentRevisions /\ environmentRevision \in EnvironmentRevisions

ReservationPrecedesActivity == activityEpoch > 0 => reservationPersisted

ReservationCannotExecute == dispatchState \in {"Reserved", "ReservationLeased"} =>
    runtimeState = "Absent" /\ ~pendingTool /\ ~outputCommitted

ExecutableRunHasActivity == dispatchState \in {"Pending", "Leased", "Awaiting", "Removed"} =>
    activityEpoch > 0

DispatchLeaseHasExactlyOneOwner ==
    (dispatchState \in {"ReservationLeased", "Leased"}) \equiv (dispatchOwner \in Workers)

WorkLeaseHasExactlyOneOwner == (workState = "Leased") \equiv (workOwner \in Workers)

RealizationLeaseHasOwner == realizationState \in {"Leased", "Ready"} =>
    realizationOwner \in Workers

ExecutionUsesExactPhysicalAuthority == runtimeState = "Running" =>
    /\ dispatchState = "Leased"
    /\ dispatchOwner = workOwner
    /\ dispatchOwner = realizationOwner
    /\ realizationState = "Ready"

ToolReplyConsumesOnePending == toolReplyCommitted => ~pendingTool

PendingToolIsCommittedAwaiting == pendingTool =>
    dispatchState = "Awaiting" /\ runtimeState = "Awaiting"

OutputRequiresTerminalCommit == outputCommitted => runtimeState \in {"Ended", "Settled"}

TerminalCommitUsesExactClaim == runtimeState = "Ended" =>
    terminalOwner = dispatchOwner /\ terminalEpoch = dispatchEpoch

SettledClearsDispatchAuthority == dispatchState = "Removed" =>
    dispatchOwner = NoWorker /\ materializedRevision = NoRevision /\ outputCommitted

Safety ==
    /\ TypeOK
    /\ SessionPinsAreImmutable
    /\ ReservationPrecedesActivity
    /\ ReservationCannotExecute
    /\ ExecutableRunHasActivity
    /\ DispatchLeaseHasExactlyOneOwner
    /\ WorkLeaseHasExactlyOneOwner
    /\ RealizationLeaseHasOwner
    /\ ExecutionUsesExactPhysicalAuthority
    /\ ToolReplyConsumesOnePending
    /\ PendingToolIsCommittedAwaiting
    /\ OutputRequiresTerminalCommit
    /\ TerminalCommitUsesExactClaim
    /\ SettledClearsDispatchAuthority
=============================================================================
