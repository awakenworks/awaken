-------------------- MODULE ObservationReconcile --------------------
EXTENDS Naturals

\* Worker observation evidence may return to old content (A -> B -> A), but
\* production pairs it with observation_sequence: the accepted heartbeat
\* sequence of the latest semantic change. The resulting epoch is strictly
\* increasing while unchanged heartbeats preserve it. Two callers model the
\* heartbeat and periodic reconciliation paths sharing one async mutex.
CONSTANTS Callers, EvidenceValues, InitialEvidence, NoCaller, MaxEpoch

KernelAssumptions ==
    /\ NoCaller \notin Callers
    /\ Callers # {}
    /\ InitialEvidence \in EvidenceValues
    /\ MaxEpoch \in Nat \ {0}
    /\ 0 \in 0..MaxEpoch

Phases == {"idle", "waiting", "reconciling", "done"}

VARIABLES evidence, sourceEpoch, publishedEvidence, publishedEpoch,
          hasFence, lastFence, captured, phase, lockOwner

vars == <<evidence, sourceEpoch, publishedEvidence, publishedEpoch,
          hasFence, lastFence, captured, phase, lockOwner>>

Init ==
    /\ evidence = InitialEvidence
    /\ sourceEpoch = 0
    /\ publishedEvidence = InitialEvidence
    /\ publishedEpoch = 0
    /\ hasFence = FALSE
    /\ lastFence = 0
    /\ captured = [caller \in Callers |-> 0]
    /\ phase = [caller \in Callers |-> "idle"]
    /\ lockOwner = NoCaller

EvidenceChanges(nextEvidence) ==
    /\ nextEvidence \in EvidenceValues \ {evidence}
    /\ sourceEpoch < MaxEpoch
    /\ evidence' = nextEvidence
    /\ sourceEpoch' = sourceEpoch + 1
    /\ UNCHANGED <<publishedEvidence, publishedEpoch, hasFence,
                    lastFence, captured, phase, lockOwner>>

\* An accepted heartbeat whose dynamic evidence is identical does not spend a
\* new observation epoch, so the production optimization still coalesces it.
UnchangedHeartbeat == UNCHANGED vars

Capture(caller) ==
    /\ caller \in Callers
    /\ phase[caller] = "idle"
    /\ captured' = [captured EXCEPT ![caller] = sourceEpoch]
    /\ phase' = [phase EXCEPT ![caller] = "waiting"]
    /\ UNCHANGED <<evidence, sourceEpoch, publishedEvidence, publishedEpoch,
                    hasFence, lastFence, lockOwner>>

Skip(caller) ==
    /\ caller \in Callers
    /\ phase[caller] = "waiting"
    /\ lockOwner = NoCaller
    /\ hasFence
    /\ lastFence = captured[caller]
    /\ phase' = [phase EXCEPT ![caller] = "done"]
    /\ UNCHANGED <<evidence, sourceEpoch, publishedEvidence, publishedEpoch,
                    hasFence, lastFence, captured, lockOwner>>

StartReconcile(caller) ==
    /\ caller \in Callers
    /\ phase[caller] = "waiting"
    /\ lockOwner = NoCaller
    /\ ~(hasFence /\ lastFence = captured[caller])
    /\ lockOwner' = caller
    /\ phase' = [phase EXCEPT ![caller] = "reconciling"]
    /\ UNCHANGED <<evidence, sourceEpoch, publishedEvidence, publishedEpoch,
                    hasFence, lastFence, captured>>

ReconcileSucceeds(caller) ==
    /\ caller \in Callers
    /\ lockOwner = caller
    /\ phase[caller] = "reconciling"
    /\ publishedEvidence' = evidence
    /\ publishedEpoch' = sourceEpoch
    /\ hasFence' = TRUE
    /\ lastFence' = captured[caller]
    /\ lockOwner' = NoCaller
    /\ phase' = [phase EXCEPT ![caller] = "done"]
    /\ UNCHANGED <<evidence, sourceEpoch, captured>>

ReconcileFails(caller) ==
    /\ caller \in Callers
    /\ lockOwner = caller
    /\ phase[caller] = "reconciling"
    /\ lockOwner' = NoCaller
    /\ phase' = [phase EXCEPT ![caller] = "done"]
    /\ UNCHANGED <<evidence, sourceEpoch, publishedEvidence, publishedEpoch,
                    hasFence, lastFence, captured>>

Reset(caller) ==
    /\ caller \in Callers
    /\ phase[caller] = "done"
    /\ phase' = [phase EXCEPT ![caller] = "idle"]
    /\ UNCHANGED <<evidence, sourceEpoch, publishedEvidence, publishedEpoch,
                    hasFence, lastFence, captured, lockOwner>>

Next ==
    \/ \E nextEvidence \in EvidenceValues: EvidenceChanges(nextEvidence)
    \/ UnchangedHeartbeat
    \/ \E caller \in Callers: Capture(caller)
    \/ \E caller \in Callers: Skip(caller)
    \/ \E caller \in Callers: StartReconcile(caller)
    \/ \E caller \in Callers: ReconcileSucceeds(caller)
    \/ \E caller \in Callers: ReconcileFails(caller)
    \/ \E caller \in Callers: Reset(caller)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ evidence \in EvidenceValues
    /\ sourceEpoch \in 0..MaxEpoch
    /\ publishedEvidence \in EvidenceValues
    /\ publishedEpoch \in 0..MaxEpoch
    /\ hasFence \in BOOLEAN
    /\ lastFence \in 0..MaxEpoch
    /\ captured \in [Callers -> 0..MaxEpoch]
    /\ phase \in [Callers -> Phases]
    /\ lockOwner \in Callers \cup {NoCaller}

CapturedNeverFuture ==
    \A caller \in Callers: captured[caller] <= sourceEpoch

PublishedNeverFuture == publishedEpoch <= sourceEpoch

PublishedCurrentWhenCaughtUp ==
    publishedEpoch = sourceEpoch => publishedEvidence = evidence

\* A skip fence can name only evidence that was reconciled already or was
\* superseded by a later reconciliation. It can never get ahead of publication.
FenceNeverAheadOfPublication == ~hasFence \/ lastFence <= publishedEpoch

MutexCoherent ==
    \A caller \in Callers:
        (phase[caller] = "reconciling") \equiv (lockOwner = caller)

Safety ==
    /\ TypeOK
    /\ CapturedNeverFuture
    /\ PublishedNeverFuture
    /\ PublishedCurrentWhenCaughtUp
    /\ FenceNeverAheadOfPublication
    /\ MutexCoherent

======================================================================
