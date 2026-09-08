--------------------------- MODULE RemoteAttempt ---------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS EndpointA, EndpointB, Task0, Task1, NoEndpoint, NoTask, MaxSends,
          MaxReads, MaxCancels

ASSUME /\ EndpointA # EndpointB
       /\ Task0 # Task1
       /\ NoEndpoint \notin {EndpointA, EndpointB}
       /\ NoTask \notin {Task0, Task1}
       /\ MaxSends \in Nat \ {0}
       /\ MaxReads \in Nat \ {0}
       /\ MaxCancels \in Nat \ {0}

Phases == {"Ready", "FreshPrepared", "FreshSent", "Running", "Recovering",
           "Awaiting", "ResumePrepared", "ResumeSent", "Unconfirmed",
           "Terminal", "Cancelled", "FailedClosed"}
Endpoints == {EndpointA, EndpointB}
Tasks == {Task0, Task1}
Operations == {"NoOperation", "Execute", "Resume"}
MessageIds == {"Execute", "Resume"}

VARIABLES phase, configuredEndpoint, durableEndpoint, durableTask,
          preparedOperation, acceptedOperation, predecessorTask,
          sentMessageIds, sends, reads, cancels, mismatchOutbound,
          cancelAddressedTask

vars == <<phase, configuredEndpoint, durableEndpoint, durableTask,
          preparedOperation, acceptedOperation, predecessorTask,
          sentMessageIds, sends, reads, cancels, mismatchOutbound,
          cancelAddressedTask>>

Init ==
    /\ phase = "Ready"
    /\ configuredEndpoint = EndpointA
    /\ durableEndpoint = NoEndpoint
    /\ durableTask = NoTask
    /\ preparedOperation = "NoOperation"
    /\ acceptedOperation = "NoOperation"
    /\ predecessorTask = NoTask
    /\ sentMessageIds = {}
    /\ sends = 0
    /\ reads = 0
    /\ cancels = 0
    /\ mismatchOutbound = FALSE
    /\ cancelAddressedTask = NoTask

\* The sole StateCell first records the exact operation coordinates. A failed
\* commit remains Ready and cannot reach the external send action.
PrepareFresh ==
    /\ phase = "Ready"
    /\ phase' = "FreshPrepared"
    /\ durableEndpoint' = configuredEndpoint
    /\ preparedOperation' = "Execute"
    /\ UNCHANGED <<configuredEndpoint, durableTask, acceptedOperation,
                    predecessorTask, sentMessageIds, sends, reads, cancels,
                    mismatchOutbound, cancelAddressedTask>>

SendFresh ==
    /\ phase = "FreshPrepared"
    /\ preparedOperation = "Execute"
    /\ "Execute" \notin sentMessageIds
    /\ sends < MaxSends
    /\ phase' = "FreshSent"
    /\ sends' = sends + 1
    /\ sentMessageIds' = sentMessageIds \cup {"Execute"}
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

\* A process can disappear on either side of send. Both gaps retain Prepared,
\* have no exact task id, and therefore enter the same fail-safe state. There
\* is intentionally no outbound transition from Unconfirmed.
CrashPrepared ==
    /\ phase \in {"FreshPrepared", "ResumePrepared"}
    /\ phase' = "Unconfirmed"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

CrashSent ==
    /\ phase \in {"FreshSent", "ResumeSent"}
    /\ phase' = "Unconfirmed"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

CommitFreshAcceptance ==
    /\ phase = "FreshSent"
    /\ phase' = "Running"
    /\ durableTask' = Task0
    /\ preparedOperation' = "NoOperation"
    /\ acceptedOperation' = "Execute"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

CrashWithAcceptedTask ==
    /\ phase = "Running"
    /\ phase' = "Recovering"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

\* Reattachment performs tasks/get only. It cannot send a second user message.
RecoverMatchingAcceptedTask ==
    /\ phase = "Recovering"
    /\ configuredEndpoint = durableEndpoint
    /\ durableTask # NoTask
    /\ reads < MaxReads
    /\ phase' = "Running"
    /\ reads' = reads + 1
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, cancels, mismatchOutbound,
                    cancelAddressedTask>>

\* A changed route cannot redirect or retire a pinned remote responsibility.
RejectEndpointMismatch ==
    /\ phase \in {"FreshPrepared", "FreshSent", "Running", "Recovering",
                    "Awaiting", "ResumePrepared", "ResumeSent", "Unconfirmed"}
    /\ durableEndpoint # NoEndpoint
    /\ configuredEndpoint # durableEndpoint
    /\ phase' = "FailedClosed"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

RouteChange ==
    /\ phase \notin {"Terminal", "Cancelled", "FailedClosed"}
    /\ configuredEndpoint' = IF configuredEndpoint = EndpointA
                              THEN EndpointB ELSE EndpointA
    /\ UNCHANGED <<phase, durableEndpoint, durableTask, preparedOperation,
                    acceptedOperation, predecessorTask, sentMessageIds, sends,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

AwaitInput ==
    /\ phase = "Running"
    /\ phase' = "Awaiting"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

\* Resume overwrites Accepted A with Prepared B, but carries A only as a
\* predecessor coordinate. From this point A cannot prove that B stopped.
PrepareResume ==
    /\ phase = "Awaiting"
    /\ configuredEndpoint = durableEndpoint
    /\ durableTask # NoTask
    /\ phase' = "ResumePrepared"
    /\ predecessorTask' = durableTask
    /\ durableTask' = NoTask
    /\ preparedOperation' = "Resume"
    /\ acceptedOperation' = "NoOperation"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, sentMessageIds, sends,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

SendResume ==
    /\ phase = "ResumePrepared"
    /\ preparedOperation = "Resume"
    /\ predecessorTask # NoTask
    /\ "Resume" \notin sentMessageIds
    /\ sends < MaxSends
    /\ phase' = "ResumeSent"
    /\ sends' = sends + 1
    /\ sentMessageIds' = sentMessageIds \cup {"Resume"}
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

CommitResumeAcceptance ==
    /\ phase = "ResumeSent"
    /\ phase' = "Running"
    /\ durableTask' = Task1
    /\ preparedOperation' = "NoOperation"
    /\ acceptedOperation' = "Resume"
    /\ predecessorTask' = NoTask
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, sentMessageIds, sends,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

Complete ==
    /\ phase = "Running"
    /\ phase' = "Terminal"
    /\ durableEndpoint' = NoEndpoint
    /\ durableTask' = NoTask
    /\ acceptedOperation' = "NoOperation"
    /\ UNCHANGED <<configuredEndpoint, preparedOperation, predecessorTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

Cancel ==
    /\ phase \in {"Running", "Awaiting", "Recovering"}
    /\ configuredEndpoint = durableEndpoint
    /\ durableTask # NoTask
    /\ preparedOperation = "NoOperation"
    /\ acceptedOperation # "NoOperation"
    /\ cancels < MaxCancels
    /\ phase' = "Cancelled"
    /\ cancels' = cancels + 1
    /\ cancelAddressedTask' = durableTask
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    preparedOperation, acceptedOperation, predecessorTask,
                    sentMessageIds, sends, reads, mismatchOutbound>>

Next ==
    \/ PrepareFresh
    \/ SendFresh
    \/ CrashPrepared
    \/ CrashSent
    \/ CommitFreshAcceptance
    \/ CrashWithAcceptedTask
    \/ RecoverMatchingAcceptedTask
    \/ RejectEndpointMismatch
    \/ RouteChange
    \/ AwaitInput
    \/ PrepareResume
    \/ SendResume
    \/ CommitResumeAcceptance
    \/ Complete
    \/ Cancel

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in Phases
    /\ configuredEndpoint \in Endpoints
    /\ durableEndpoint \in Endpoints \cup {NoEndpoint}
    /\ durableTask \in Tasks \cup {NoTask}
    /\ preparedOperation \in Operations
    /\ acceptedOperation \in Operations
    /\ predecessorTask \in Tasks \cup {NoTask}
    /\ sentMessageIds \subseteq MessageIds
    /\ sends \in 0..MaxSends
    /\ reads \in 0..MaxReads
    /\ cancels \in 0..MaxCancels
    /\ mismatchOutbound \in BOOLEAN
    /\ cancelAddressedTask \in Tasks \cup {NoTask}

PreparedHasNoAddressableTask ==
    phase \in {"FreshPrepared", "FreshSent", "ResumePrepared", "ResumeSent",
               "Unconfirmed"}
        => /\ preparedOperation \in {"Execute", "Resume"}
           /\ acceptedOperation = "NoOperation"
           /\ durableEndpoint # NoEndpoint
           /\ durableTask = NoTask

AcceptedLifecycleHasExactTask ==
    phase \in {"Running", "Recovering", "Awaiting", "Cancelled"}
        => /\ preparedOperation = "NoOperation"
           /\ acceptedOperation \in {"Execute", "Resume"}
           /\ durableEndpoint # NoEndpoint
           /\ durableTask # NoTask

ResumePreparedRetainsOnlyPredecessorCoordinate ==
    phase \in {"ResumePrepared", "ResumeSent"}
        => /\ predecessorTask # NoTask
           /\ durableTask = NoTask

UnconfirmedCannotClaimAcceptedTask ==
    phase = "Unconfirmed"
        => /\ preparedOperation \in {"Execute", "Resume"}
           /\ acceptedOperation = "NoOperation"
           /\ durableTask = NoTask

TerminalClearsOperation ==
    phase = "Terminal"
        => /\ durableEndpoint = NoEndpoint
           /\ durableTask = NoTask
           /\ acceptedOperation = "NoOperation"

StableReplayIdentity == sentMessageIds \subseteq {"Execute", "Resume"}

EndpointMismatchFailsClosed ==
    phase = "FailedClosed" => configuredEndpoint # durableEndpoint

MismatchNeverProducesOutbound == ~mismatchOutbound

CancellationTargetsOnlyAcceptedTask ==
    cancels > 0
        => /\ cancelAddressedTask = durableTask
           /\ acceptedOperation # "NoOperation"
           /\ preparedOperation = "NoOperation"

Safety ==
    /\ TypeOK
    /\ PreparedHasNoAddressableTask
    /\ AcceptedLifecycleHasExactTask
    /\ ResumePreparedRetainsOnlyPredecessorCoordinate
    /\ UnconfirmedCannotClaimAcceptedTask
    /\ TerminalClearsOperation
    /\ StableReplayIdentity
    /\ EndpointMismatchFailsClosed
    /\ MismatchNeverProducesOutbound
    /\ CancellationTargetsOnlyAcceptedTask

=============================================================================
