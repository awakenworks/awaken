-------------------------- MODULE BackgroundTask --------------------------
EXTENDS Naturals

CONSTANTS Workers, Lease, MaxAttempts, MaxTime

Phases == {"Requested", "Running", "Waiting", "Cancelling", "Ended"}
Modes == {"Replay", "NeverReplay"}
TerminalReasons == {"None", "Succeeded", "Failed", "Cancelled", "Indeterminate"}
LivePhases == {"Running", "Waiting", "Cancelling"}

VARIABLES phase, owner, epoch, expires, now, mode, terminalReason,
          completionOwner, completionEpoch, completionReason

vars == <<phase, owner, epoch, expires, now, mode, terminalReason,
          completionOwner, completionEpoch, completionReason>>

Init ==
    /\ phase = "Requested"
    /\ owner = "None"
    /\ epoch = 0
    /\ expires = 0
    /\ now = 0
    /\ mode \in Modes
    /\ terminalReason = "None"
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
                    completionOwner, completionEpoch, completionReason>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, owner, epoch, expires, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

Heartbeat(worker, observedEpoch) ==
    /\ phase \in LivePhases
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ now + Lease > expires
    /\ expires' = now + Lease
    /\ UNCHANGED <<phase, owner, epoch, now, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

Wait(worker, observedEpoch) ==
    /\ phase = "Running"
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ phase' = "Waiting"
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

CancelRequested ==
    /\ phase = "Requested"
    /\ phase' = "Ended"
    /\ terminalReason' = "Cancelled"
    /\ UNCHANGED <<owner, epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

CancelLive ==
    /\ phase \in {"Running", "Waiting"}
    /\ phase' = "Cancelling"
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

ReclaimReplay(worker) ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ mode = "Replay"
    /\ epoch < MaxAttempts
    /\ worker \in Workers
    /\ phase' = "Running"
    /\ owner' = worker
    /\ epoch' = epoch + 1
    /\ expires' = now + Lease
    /\ UNCHANGED <<now, mode, terminalReason,
                    completionOwner, completionEpoch, completionReason>>

ReclaimNeverReplay ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ mode = "NeverReplay"
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = "Indeterminate"
    /\ UNCHANGED <<epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

ReclaimExhausted ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ mode = "Replay"
    /\ epoch >= MaxAttempts
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = "Failed"
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
    /\ UNCHANGED <<epoch, expires, now, mode,
                    completionOwner, completionEpoch, completionReason>>

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
    /\ UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason>>

LoseCompletion ==
    /\ completionReason # "None"
    /\ completionOwner' = "None"
    /\ completionEpoch' = 0
    /\ completionReason' = "None"
    /\ UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason>>

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
               /\ UNCHANGED <<epoch, expires, now, mode>>
          ELSE UNCHANGED <<phase, owner, epoch, expires, now, mode, terminalReason>>
    /\ completionOwner' = "None"
    /\ completionEpoch' = 0
    /\ completionReason' = "None"

Next ==
    \/ \E worker \in Workers : Start(worker)
    \/ Tick
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts :
        Heartbeat(worker, observedEpoch)
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts :
        Wait(worker, observedEpoch)
    \/ CancelRequested
    \/ CancelLive
    \/ \E worker \in Workers : ReclaimReplay(worker)
    \/ ReclaimNeverReplay
    \/ ReclaimExhausted
    \/ \E worker \in Workers, observedEpoch \in 0..MaxAttempts,
          reason \in {"Succeeded", "Failed"} :
        Finish(worker, observedEpoch, reason)
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

CompletionProjectionIsComplete ==
    (completionReason # "None") <=>
      /\ completionOwner \in Workers
      /\ completionEpoch > 0

CompletionProjectionNeverOutlivesItsAttemptSpace ==
    completionReason # "None" => completionEpoch <= MaxAttempts

=============================================================================
