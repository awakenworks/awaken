-------------------------- MODULE BackgroundTask --------------------------
EXTENDS Naturals

CONSTANTS Workers, Lease, MaxAttempts, MaxTime, RemoteIds, PollIntervals

Phases == {"Requested", "Running", "Waiting", "Cancelling", "Ended"}
Modes == {"Replay", "NeverReplay"}
TerminalReasons == {"None", "Succeeded", "Failed", "Cancelled", "Indeterminate"}
LivePhases == {"Running", "Waiting", "Cancelling"}
ContinuationValues == {"None", "Local"} \cup RemoteIds
PollDelays == {0} \cup PollIntervals

VARIABLES phase, owner, epoch, expires, now, mode, terminalReason,
          continuation, pollInterval, continuationHistory,
          completionOwner, completionEpoch, completionReason

vars == <<phase, owner, epoch, expires, now, mode, terminalReason,
          continuation, pollInterval, continuationHistory,
          completionOwner, completionEpoch, completionReason>>

Init ==
    /\ phase = "Requested"
    /\ owner = "None"
    /\ epoch = 0
    /\ expires = 0
    /\ now = 0
    /\ mode \in Modes
    /\ terminalReason = "None"
    /\ continuation = "None"
    /\ pollInterval = 0
    /\ continuationHistory = "None"
    /\ completionOwner = "None"
    /\ completionEpoch = 0
    /\ completionReason = "None"

Start(worker) ==
    /\ phase = "Requested"
    /\ worker \in Workers
    /\ phase' = "Running"
    /\ owner' = worker
    /\ epoch' = 1
    /\ expires' = now + Lease
    /\ UNCHANGED <<now, mode, terminalReason,
                    continuation, pollInterval, continuationHistory,
                    completionOwner, completionEpoch, completionReason>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, owner, epoch, expires, mode, terminalReason,
                    continuation, pollInterval, continuationHistory,
                    completionOwner, completionEpoch, completionReason>>

Heartbeat(worker, observedEpoch) ==
    /\ phase \in LivePhases
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ now + Lease > expires
    /\ expires' = now + Lease
    /\ UNCHANGED <<phase, owner, epoch, now, mode, terminalReason,
                    continuation, pollInterval, continuationHistory,
                    completionOwner, completionEpoch, completionReason>>

WaitLocal(worker, observedEpoch) ==
    /\ phase = "Running"
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ phase' = "Waiting"
    /\ continuation' = "Local"
    /\ pollInterval' = 0
    /\ continuationHistory' = "None"
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

WaitRemote(worker, observedEpoch, remoteId, interval) ==
    /\ phase \in {"Running", "Cancelling"}
    /\ continuation = "None"
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ remoteId \in RemoteIds
    /\ interval \in PollDelays
    /\ phase' = IF phase = "Running" THEN "Waiting" ELSE "Cancelling"
    /\ continuation' = remoteId
    /\ pollInterval' = interval
    /\ continuationHistory' = remoteId
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

\* Polling may revise only the recommended delay. It cannot exchange the
\* committed remote identity for another operation.
RefreshRemoteWait(worker, observedEpoch, interval) ==
    /\ phase \in {"Waiting", "Cancelling"}
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ continuation \in RemoteIds
    /\ interval \in PollDelays
    /\ pollInterval' = interval
    /\ UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason,
                    continuation, continuationHistory,
                    completionOwner, completionEpoch, completionReason>>

CancelRequested ==
    /\ phase = "Requested"
    /\ phase' = "Ended"
    /\ terminalReason' = "Cancelled"
    /\ continuation' = "None"
    /\ pollInterval' = 0
    /\ continuationHistory' = "None"
    /\ UNCHANGED <<owner, epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

\* A cancelling task keeps its wait coordinates so the current or replacement
\* owner can address the exact remote operation instead of cancelling locally.
CancelLive ==
    /\ phase \in {"Running", "Waiting"}
    /\ phase' = "Cancelling"
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason,
                    continuation, pollInterval, continuationHistory,
                    completionOwner, completionEpoch, completionReason>>

\* Reclaim transfers only the owner fence. A Replay policy may replay its
\* current phase. A NeverReplay task becomes reconnectable only after an exact
\* remote id is durable: recovery then polls/cancels that id, not the effect
\* that created it. Waiting stays Waiting and Cancelling stays Cancelling.
ReclaimRecoverable(worker) ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ \/ mode = "Replay"
       \/ continuation \in RemoteIds
    /\ epoch < MaxAttempts
    /\ worker \in Workers
    /\ phase' = phase
    /\ owner' = worker
    /\ epoch' = epoch + 1
    /\ expires' = now + Lease
    /\ UNCHANGED <<now, mode, terminalReason,
                    continuation, pollInterval, continuationHistory,
                    completionOwner, completionEpoch, completionReason>>

ReclaimNeverReplay ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ mode = "NeverReplay"
    /\ continuation \notin RemoteIds
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = "Indeterminate"
    /\ continuation' = "None"
    /\ pollInterval' = 0
    /\ continuationHistory' = "None"
    /\ UNCHANGED <<epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

ReclaimExhausted ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ \/ mode = "Replay"
       \/ continuation \in RemoteIds
    /\ epoch >= MaxAttempts
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = IF continuation \in RemoteIds
                          THEN "Indeterminate"
                          ELSE "Failed"
    /\ continuation' = "None"
    /\ pollInterval' = 0
    /\ continuationHistory' = "None"
    /\ UNCHANGED <<epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

Finish(worker, observedEpoch, reason) ==
    /\ phase \in LivePhases
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ reason \in {"Succeeded", "Failed"}
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = IF phase = "Cancelling" THEN "Cancelled" ELSE reason
    /\ continuation' = "None"
    /\ pollInterval' = 0
    /\ continuationHistory' = "None"
    /\ UNCHANGED <<epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

\* An observation for a previous owner or epoch is an explicit inert action.
RejectStaleSettlement(worker, observedEpoch) ==
    /\ phase \in LivePhases
    /\ worker \in Workers
    /\ \/ worker # owner
       \/ observedEpoch # epoch
    /\ UNCHANGED vars

\* The process supervisor may remember one completion only after the exact
\* owner/fence finishes. This is retryable projection evidence, not durable
\* task truth: it may disappear in a process crash without changing the task.
RecordCompletion(worker, observedEpoch, reason) ==
    /\ phase \in LivePhases
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ reason \in {"Succeeded", "Failed"}
    /\ completionReason = "None"
    /\ completionOwner' = worker
    /\ completionEpoch' = observedEpoch
    /\ completionReason' = reason
    /\ UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason,
                    continuation, pollInterval, continuationHistory>>

LoseCompletion ==
    /\ completionReason # "None"
    /\ completionOwner' = "None"
    /\ completionEpoch' = 0
    /\ completionReason' = "None"
    /\ UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason,
                    continuation, pollInterval, continuationHistory>>

\* StepStart folds a matching completion into the ordinary durable aggregate.
\* A completion made stale by reclaim is retired without mutating durable truth.
FoldCompletion ==
    /\ completionReason # "None"
    /\ IF phase \in LivePhases
          /\ completionOwner = owner
          /\ completionEpoch = epoch
          THEN /\ phase' = "Ended"
               /\ owner' = "None"
               /\ terminalReason' = IF phase = "Cancelling"
                                       THEN "Cancelled"
                                       ELSE completionReason
               /\ continuation' = "None"
               /\ pollInterval' = 0
               /\ continuationHistory' = "None"
               /\ UNCHANGED <<epoch, expires, now, mode>>
          ELSE UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason,
                           continuation, pollInterval, continuationHistory>>
    /\ completionOwner' = "None"
    /\ completionEpoch' = 0
    /\ completionReason' = "None"

Next ==
    \/ \E worker \in Workers : Start(worker)
    \/ Tick
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts :
        Heartbeat(worker, observedEpoch)
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts :
        WaitLocal(worker, observedEpoch)
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts,
          remoteId \in RemoteIds, interval \in PollDelays :
        WaitRemote(worker, observedEpoch, remoteId, interval)
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts,
          interval \in PollDelays :
        RefreshRemoteWait(worker, observedEpoch, interval)
    \/ CancelRequested
    \/ CancelLive
    \/ \E worker \in Workers : ReclaimRecoverable(worker)
    \/ ReclaimNeverReplay
    \/ ReclaimExhausted
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts,
          reason \in {"Succeeded", "Failed"} :
        Finish(worker, observedEpoch, reason)
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts :
        RejectStaleSettlement(worker, observedEpoch)
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts,
          reason \in {"Succeeded", "Failed"} :
        RecordCompletion(worker, observedEpoch, reason)
    \/ LoseCompletion
    \/ FoldCompletion

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in Phases
    /\ owner \in Workers \cup {"None"}
    /\ epoch \in Nat
    /\ expires \in Nat
    /\ now \in Nat
    /\ mode \in Modes
    /\ terminalReason \in TerminalReasons
    /\ continuation \in ContinuationValues
    /\ pollInterval \in Nat
    /\ continuationHistory \in RemoteIds \cup {"None"}
    /\ completionOwner \in Workers \cup {"None"}
    /\ completionEpoch \in Nat
    /\ completionReason \in {"None", "Succeeded", "Failed"}

OwnerExistsExactlyWhileLive ==
    (owner \in Workers) <=> (phase \in LivePhases)

RequestedHasNoAttempt ==
    phase = "Requested" => /\ epoch = 0 /\ expires = 0 /\ owner = "None"

LiveAttemptIsFenced ==
    phase \in LivePhases => /\ epoch > 0 /\ epoch <= MaxAttempts /\ expires > 0

TerminalClearsOwner ==
    phase = "Ended" => /\ owner = "None" /\ terminalReason # "None"

CancellationWinsMatchingFinish ==
    phase = "Ended" /\ terminalReason = "Cancelled" => owner = "None"

WaitingAlwaysCarriesAWait ==
    phase = "Waiting" => continuation # "None"

ContinuationExistsOnlyWhileWaitingOrCancelling ==
    continuation # "None" => phase \in {"Waiting", "Cancelling"}

\* continuationHistory is a proof-only witness. Once a remote task has entered
\* durable Waiting it remains exactly addressable through cancel and reclaim;
\* only a terminal transition may clear both values.
RemoteContinuationIsNeverLostOrSubstituted ==
    /\ continuation \in RemoteIds =>
         /\ continuationHistory = continuation
         /\ pollInterval \in PollDelays
    /\ continuation \notin RemoteIds =>
         /\ continuationHistory = "None"
         /\ pollInterval = 0

CompletionProjectionIsComplete ==
    (completionReason # "None") <=>
      /\ completionOwner \in Workers
      /\ completionEpoch > 0

CompletionProjectionNeverOutlivesItsAttemptSpace ==
    completionReason # "None" => completionEpoch <= MaxAttempts

=============================================================================
