------------------ MODULE ManagedCredentialCreation ------------------
EXTENDS Naturals, TLC

CONSTANT MaxOwnerEpoch
ASSUME MaxOwnerEpoch \in Nat /\ MaxOwnerEpoch >= 2

\* Bounded abstraction of the Rust PendingManagedCredentialMutation protocol.
\* ownerEpoch represents the durable (writer_token, writer_epoch) fence and
\* leaseExpired represents deadline <= injected now. Legacy JSON starts with
\* epoch zero and an expired lease, then follows the same atomic claim path.
VARIABLE phase, intentId, material, sourcePublished, childPublished,
         processUp, owner, ownerEpoch, leaseExpired, liveWriterActive,
         admitted, duplicateSeen, conflictRejected, staleRejected,
         recoveryClaimed

vars == <<phase, intentId, material, sourcePublished, childPublished,
          processUp, owner, ownerEpoch, leaseExpired, liveWriterActive,
          admitted, duplicateSeen, conflictRejected, staleRejected,
          recoveryClaimed>>

IntentIds == {"intent-a", "intent-b"}
Owners == {"None", "Live", "Recovery"}
TerminalPhases == {"Published", "Aborted"}

Init == /\ phase = "None" /\ intentId = "None" /\ material = "None"
        /\ sourcePublished = FALSE /\ childPublished = FALSE
        /\ processUp = TRUE /\ owner = "None" /\ ownerEpoch = 0
        /\ leaseExpired = TRUE /\ liveWriterActive = FALSE
        /\ admitted \in BOOLEAN
        /\ duplicateSeen = FALSE /\ conflictRejected = FALSE
        /\ staleRejected = FALSE /\ recoveryClaimed = FALSE

Begin(id) ==
    /\ processUp /\ phase = "None" /\ id \in IntentIds
    /\ phase' = "Writing" /\ intentId' = id /\ owner' = "Live"
    /\ ownerEpoch' = 1 /\ leaseExpired' = FALSE
    /\ liveWriterActive' = TRUE
    /\ UNCHANGED <<material, sourcePublished, childPublished, processUp,
                    admitted, duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed>>

DuplicateBegin(id) ==
    /\ id = intentId /\ phase # "None"
    /\ duplicateSeen' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted,
                    conflictRejected, staleRejected, recoveryClaimed>>

ConflictingBegin(id) ==
    /\ id \in IntentIds /\ intentId \in IntentIds /\ id # intentId
    /\ phase # "None"
    /\ conflictRejected' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    staleRejected, recoveryClaimed>>

PutPartial(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material = "None"
    /\ epoch = ownerEpoch
    /\ (owner = "Recovery" \/ (owner = "Live" /\ liveWriterActive))
    /\ material' = "Partial"
    /\ UNCHANGED <<phase, intentId, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

PutComplete(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material \in {"None", "Partial"}
    /\ epoch = ownerEpoch
    /\ (owner = "Recovery" \/ (owner = "Live" /\ liveWriterActive))
    /\ material' = "Complete"
    /\ UNCHANGED <<phase, intentId, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

MarkReady(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material = "Complete"
    /\ epoch = ownerEpoch
    /\ (owner = "Recovery" \/ (owner = "Live" /\ liveWriterActive))
    /\ phase' = "Ready" /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<intentId, material, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed>>

AbortIncomplete(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material \in {"None", "Partial"}
    /\ epoch = ownerEpoch /\ owner = "Recovery"
    /\ phase' = "ReclaimingAbort"
    /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<intentId, material, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

Commit(epoch) ==
    /\ processUp /\ phase = "Ready" /\ material = "Complete"
    /\ epoch = ownerEpoch /\ owner \in {"Live", "Recovery"}
    /\ admitted = TRUE
    /\ phase' = "Published" /\ sourcePublished' = TRUE
    /\ childPublished' = TRUE /\ owner' = "None"
    /\ UNCHANGED <<intentId, material, processUp, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

Reject(epoch) ==
    /\ processUp /\ phase = "Ready" /\ material = "Complete"
    /\ epoch = ownerEpoch /\ owner \in {"Live", "Recovery"}
    /\ admitted = FALSE
    /\ phase' = "ReclaimingAbort" /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<intentId, material, sourcePublished, childPublished, processUp, owner,
                    ownerEpoch, leaseExpired, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed>>

\* External deletion is retryable because ReclaimingAbort remains durable
\* across every crash prefix. Only exact cleanup completion retires the fact.
CompleteAbortCleanup ==
    /\ processUp /\ phase = "ReclaimingAbort"
    /\ phase' = "Aborted" /\ material' = "None" /\ owner' = "None"
    /\ UNCHANGED <<intentId, sourcePublished, childPublished, processUp,
                    ownerEpoch, leaseExpired, liveWriterActive, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed>>

CancelLiveWriter ==
    /\ phase = "Writing" /\ owner = "Live" /\ liveWriterActive
    /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, admitted, duplicateSeen, conflictRejected,
                    staleRejected, recoveryClaimed>>

\* Abstract deadline passage. Before this action the periodic reconciler has no
\* takeover transition, exactly matching deadline > injected-now in Rust.
ExpireLiveLease ==
    /\ phase = "Writing" /\ owner = "Live" /\ ~leaseExpired
    /\ leaseExpired' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

ClaimExpiredWriting ==
    /\ processUp /\ phase = "Writing" /\ leaseExpired
    /\ ownerEpoch < MaxOwnerEpoch
    /\ owner' = "Recovery" /\ ownerEpoch' = ownerEpoch + 1
    /\ leaseExpired' = FALSE /\ liveWriterActive' = FALSE
    /\ recoveryClaimed' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, admitted, duplicateSeen,
                    conflictRejected, staleRejected>>

\* Old tokens may attempt both the external-write transition and publication;
\* neither can change durable material/phase/pair truth after takeover.
StaleWrite(epoch) ==
    /\ phase = "Writing" /\ epoch \in 0..MaxOwnerEpoch
    /\ epoch # ownerEpoch
    /\ staleRejected' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, recoveryClaimed>>

StaleCommit(epoch) ==
    /\ phase \in {"Ready", "ReclaimingAbort", "Published"}
    /\ epoch \in 0..MaxOwnerEpoch
    /\ epoch # ownerEpoch
    /\ staleRejected' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, recoveryClaimed>>

Crash ==
    /\ processUp /\ phase \in {"Writing", "Ready", "ReclaimingAbort"}
    /\ processUp' = FALSE /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, owner, ownerEpoch, leaseExpired, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed>>

Restart ==
    /\ ~processUp /\ processUp' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, owner, ownerEpoch, leaseExpired,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

Quiesce == /\ phase \in TerminalPhases /\ UNCHANGED vars

PutPartialAny == \E epoch \in 0..MaxOwnerEpoch: PutPartial(epoch)
PutCompleteAny == \E epoch \in 0..MaxOwnerEpoch: PutComplete(epoch)
MarkReadyAny == \E epoch \in 0..MaxOwnerEpoch: MarkReady(epoch)
AbortIncompleteAny == \E epoch \in 0..MaxOwnerEpoch: AbortIncomplete(epoch)
CommitAny == \E epoch \in 0..MaxOwnerEpoch: Commit(epoch)
RejectAny == \E epoch \in 0..MaxOwnerEpoch: Reject(epoch)
StaleWriteAny == \E epoch \in 0..MaxOwnerEpoch: StaleWrite(epoch)
StaleCommitAny == \E epoch \in 0..MaxOwnerEpoch: StaleCommit(epoch)

Next == (\E id \in IntentIds: Begin(id) \/ DuplicateBegin(id) \/ ConflictingBegin(id))
        \/ PutPartialAny \/ PutCompleteAny \/ MarkReadyAny
        \/ AbortIncompleteAny \/ CommitAny \/ RejectAny
        \/ CompleteAbortCleanup
        \/ CancelLiveWriter \/ ExpireLiveLease \/ ClaimExpiredWriting
        \/ StaleWriteAny \/ StaleCommitAny \/ Crash \/ Restart \/ Quiesce

TypeOK == /\ phase \in {"None", "Writing", "Ready", "ReclaimingAbort",
                         "Published", "Aborted"}
          /\ intentId \in IntentIds \cup {"None"}
          /\ material \in {"None", "Partial", "Complete"}
          /\ sourcePublished \in BOOLEAN /\ childPublished \in BOOLEAN
          /\ processUp \in BOOLEAN /\ owner \in Owners
          /\ ownerEpoch \in 0..MaxOwnerEpoch /\ leaseExpired \in BOOLEAN
          /\ liveWriterActive \in BOOLEAN /\ admitted \in BOOLEAN
          /\ duplicateSeen \in BOOLEAN /\ conflictRejected \in BOOLEAN
          /\ staleRejected \in BOOLEAN /\ recoveryClaimed \in BOOLEAN

SourceAndChildPublishAtomically == sourcePublished = childPublished
PublishedPairHasCompleteMaterial ==
    sourcePublished => phase = "Published" /\ material = "Complete" /\ intentId \in IntentIds
AbortedCreationIsClean ==
    phase = "Aborted" => ~sourcePublished /\ ~childPublished /\ material = "None"
UnpublishedMaterialCleanupHasDurableFact ==
    (material # "None" /\ ~sourcePublished) =>
        phase \in {"Writing", "Ready", "ReclaimingAbort"}
WritingAlwaysHasDurableOwner ==
    phase = "Writing" => owner \in {"Live", "Recovery"} /\ ownerEpoch > 0
RecoveryClaimChangesTheFence == recoveryClaimed => ownerEpoch >= 2
LiveLeaseIsNeverStolen == owner = "Recovery" => recoveryClaimed
IntentIdentityNeverDisappears == phase # "None" => intentId \in IntentIds

CreationSafety ==
    /\ TypeOK /\ SourceAndChildPublishAtomically
    /\ PublishedPairHasCompleteMaterial /\ AbortedCreationIsClean
    /\ UnpublishedMaterialCleanupHasDurableFact
    /\ WritingAlwaysHasDurableOwner /\ RecoveryClaimChangesTheFence
    /\ LiveLeaseIsNeverStolen /\ IntentIdentityNeverDisappears

Spec == Init /\ [][Next]_vars

FairSpec ==
    /\ Spec
    /\ WF_vars(Restart)
    /\ SF_vars(ExpireLiveLease)
    /\ SF_vars(ClaimExpiredWriting)
    /\ SF_vars(AbortIncompleteAny)
    /\ SF_vars(CompleteAbortCleanup)
    /\ SF_vars(MarkReadyAny)
    /\ SF_vars(CommitAny)
    /\ SF_vars(RejectAny)

AbandonedWritingEventuallyConverges ==
    (phase = "Writing" /\ ~liveWriterActive) ~> phase \in TerminalPhases
======================================================================
