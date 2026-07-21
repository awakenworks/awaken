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

Phases == {"Ready", "Sent", "Running", "Recovering", "Awaiting",
           "ResumeSent", "Terminal", "Cancelled", "FailedClosed"}
Endpoints == {EndpointA, EndpointB}
Tasks == {Task0, Task1}
MessageIds == {"Execute", "Resume"}

VARIABLES phase, configuredEndpoint, durableEndpoint, durableTask,
          sentMessageIds, sends, reads, cancels, mismatchOutbound,
          cancelAddressedTask

vars == <<phase, configuredEndpoint, durableEndpoint, durableTask,
          sentMessageIds, sends, reads, cancels, mismatchOutbound,
          cancelAddressedTask>>

Init ==
    /\ phase = "Ready"
    /\ configuredEndpoint = EndpointA
    /\ durableEndpoint = NoEndpoint
    /\ durableTask = NoTask
    /\ sentMessageIds = {}
    /\ sends = 0
    /\ reads = 0
    /\ cancels = 0
    /\ mismatchOutbound = FALSE
    /\ cancelAddressedTask = NoTask

\* `message:send` is an external effect and therefore precedes the local
\* ThreadCommit. A crash may expose this gap. Every replay nevertheless uses
\* the same Run-derived message id; remote deduplication is an explicit peer
\* contract, not an exactly-once claim made by this model.
SendFresh ==
    /\ phase = "Ready"
    /\ sends < MaxSends
    /\ phase' = "Sent"
    /\ sends' = sends + 1
    /\ sentMessageIds' = sentMessageIds \cup {"Execute"}
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask, reads,
                    cancels, mismatchOutbound, cancelAddressedTask>>

CrashBeforeFreshCommit ==
    /\ phase = "Sent"
    /\ phase' = "Ready"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

CommitFreshReference ==
    /\ phase = "Sent"
    /\ phase' = "Running"
    /\ durableEndpoint' = configuredEndpoint
    /\ durableTask' = Task0
    /\ UNCHANGED <<configuredEndpoint, sentMessageIds, sends, reads, cancels,
                    mismatchOutbound, cancelAddressedTask>>

CrashWithReference ==
    /\ phase = "Running"
    /\ phase' = "Recovering"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

\* Reattachment performs tasks/get only. It cannot send a second user message.
RecoverMatchingReference ==
    /\ phase = "Recovering"
    /\ configuredEndpoint = durableEndpoint
    /\ reads < MaxReads
    /\ phase' = "Running"
    /\ reads' = reads + 1
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, cancels, mismatchOutbound,
                    cancelAddressedTask>>

\* A changed route cannot redirect a pinned remote task to another endpoint.
RecoverMismatchedReference ==
    /\ phase = "Recovering"
    /\ configuredEndpoint # durableEndpoint
    /\ phase' = "FailedClosed"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

RouteChange ==
    /\ phase \notin {"Terminal", "Cancelled", "FailedClosed"}
    /\ configuredEndpoint' = IF configuredEndpoint = EndpointA
                              THEN EndpointB ELSE EndpointA
    /\ UNCHANGED <<phase, durableEndpoint, durableTask, sentMessageIds, sends,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

AwaitInput ==
    /\ phase = "Running"
    /\ phase' = "Awaiting"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

SendResume ==
    /\ phase = "Awaiting"
    /\ configuredEndpoint = durableEndpoint
    /\ sends < MaxSends
    /\ phase' = "ResumeSent"
    /\ sends' = sends + 1
    /\ sentMessageIds' = sentMessageIds \cup {"Resume"}
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask, reads,
                    cancels, mismatchOutbound, cancelAddressedTask>>

CrashBeforeResumeCommit ==
    /\ phase = "ResumeSent"
    /\ phase' = "Awaiting"
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, reads, cancels, mismatchOutbound,
                    cancelAddressedTask>>

CommitResumeReference ==
    /\ phase = "ResumeSent"
    /\ phase' = "Running"
    /\ durableTask' = Task1
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, sentMessageIds, sends,
                    reads, cancels, mismatchOutbound, cancelAddressedTask>>

Complete ==
    /\ phase = "Running"
    /\ phase' = "Terminal"
    /\ durableEndpoint' = NoEndpoint
    /\ durableTask' = NoTask
    /\ UNCHANGED <<configuredEndpoint, sentMessageIds, sends, reads, cancels,
                    mismatchOutbound, cancelAddressedTask>>

Cancel ==
    /\ phase \in {"Running", "Awaiting", "Recovering"}
    /\ configuredEndpoint = durableEndpoint
    /\ durableTask # NoTask
    /\ cancels < MaxCancels
    /\ phase' = "Cancelled"
    /\ cancels' = cancels + 1
    /\ cancelAddressedTask' = durableTask
    /\ UNCHANGED <<configuredEndpoint, durableEndpoint, durableTask,
                    sentMessageIds, sends, reads, mismatchOutbound>>

Next ==
    \/ SendFresh
    \/ CrashBeforeFreshCommit
    \/ CommitFreshReference
    \/ CrashWithReference
    \/ RecoverMatchingReference
    \/ RecoverMismatchedReference
    \/ RouteChange
    \/ AwaitInput
    \/ SendResume
    \/ CrashBeforeResumeCommit
    \/ CommitResumeReference
    \/ Complete
    \/ Cancel

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in Phases
    /\ configuredEndpoint \in Endpoints
    /\ durableEndpoint \in Endpoints \cup {NoEndpoint}
    /\ durableTask \in Tasks \cup {NoTask}
    /\ sentMessageIds \subseteq MessageIds
    /\ sends \in 0..MaxSends
    /\ reads \in 0..MaxReads
    /\ cancels \in 0..MaxCancels
    /\ mismatchOutbound \in BOOLEAN
    /\ cancelAddressedTask \in Tasks \cup {NoTask}

ReferenceIsComplete ==
    (durableTask = NoTask) <=> (durableEndpoint = NoEndpoint)

ActiveRemoteLifecycleHasReference ==
    phase \in {"Running", "Recovering", "Awaiting", "ResumeSent", "Cancelled"}
        => durableTask # NoTask

TerminalClearsReference ==
    phase = "Terminal" => durableTask = NoTask

StableReplayIdentity == sentMessageIds \subseteq {"Execute", "Resume"}

EndpointMismatchFailsClosed ==
    phase = "FailedClosed" => configuredEndpoint # durableEndpoint

MismatchNeverProducesOutbound == ~mismatchOutbound

CancellationTargetsPinnedTask ==
    cancels > 0 => cancelAddressedTask = durableTask

Safety ==
    /\ TypeOK
    /\ ReferenceIsComplete
    /\ ActiveRemoteLifecycleHasReference
    /\ TerminalClearsReference
    /\ StableReplayIdentity
    /\ EndpointMismatchFailsClosed
    /\ MismatchNeverProducesOutbound
    /\ CancellationTargetsPinnedTask

=============================================================================
