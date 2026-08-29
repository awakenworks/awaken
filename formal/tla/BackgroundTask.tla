-------------------------- MODULE BackgroundTask --------------------------
EXTENDS Naturals

CONSTANTS Workers, Lease, MaxAttempts, MaxTime

Phases == {"Requested", "Running", "Waiting", "Cancelling", "Ended"}
Modes == {"Replay", "NeverReplay"}
TerminalReasons == {"None", "Succeeded", "Failed", "Cancelled", "Indeterminate"}
LivePhases == {"Running", "Waiting", "Cancelling"}

VARIABLES phase, owner, epoch, expires, now, mode, terminalReason

vars == <<phase, owner, epoch, expires, now, mode, terminalReason>>

Init ==
    /\ phase = "Requested"
    /\ owner = "None"
    /\ epoch = 0
    /\ expires = 0
    /\ now = 0
    /\ mode \in Modes
    /\ terminalReason = "None"

Start(worker) ==
    /\ phase = "Requested"
    /\ worker \in Workers
    /\ phase' = "Running"
    /\ owner' = worker
    /\ epoch' = 1
    /\ expires' = now + Lease
    /\ UNCHANGED <<now, mode, terminalReason>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, owner, epoch, expires, mode, terminalReason>>

Heartbeat(worker, observedEpoch) ==
    /\ phase \in LivePhases
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ now + Lease > expires
    /\ expires' = now + Lease
    /\ UNCHANGED <<phase, owner, epoch, now, mode, terminalReason>>

Wait(worker, observedEpoch) ==
    /\ phase = "Running"
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ phase' = "Waiting"
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason>>

CancelRequested ==
    /\ phase = "Requested"
    /\ phase' = "Ended"
    /\ terminalReason' = "Cancelled"
    /\ UNCHANGED <<owner, epoch, expires, now, mode>>

CancelLive ==
    /\ phase \in {"Running", "Waiting"}
    /\ phase' = "Cancelling"
    /\ UNCHANGED <<owner, epoch, expires, now, mode, terminalReason>>

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
    /\ UNCHANGED <<now, mode, terminalReason>>

ReclaimNeverReplay ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ mode = "NeverReplay"
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = "Indeterminate"
    /\ UNCHANGED <<epoch, expires, now, mode>>

ReclaimExhausted ==
    /\ phase \in LivePhases
    /\ expires <= now
    /\ mode = "Replay"
    /\ epoch >= MaxAttempts
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = "Failed"
    /\ UNCHANGED <<epoch, expires, now, mode>>

Finish(worker, observedEpoch, reason) ==
    /\ phase \in LivePhases
    /\ worker = owner
    /\ observedEpoch = epoch
    /\ reason \in {"Succeeded", "Failed"}
    /\ phase' = "Ended"
    /\ owner' = "None"
    /\ terminalReason' = IF phase = "Cancelling" THEN "Cancelled" ELSE reason
    /\ UNCHANGED <<epoch, expires, now, mode>>

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

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in Phases
    /\ owner \in Workers \cup {"None"}
    /\ epoch \in Nat
    /\ expires \in Nat
    /\ now \in Nat
    /\ mode \in Modes
    /\ terminalReason \in TerminalReasons

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

=============================================================================
