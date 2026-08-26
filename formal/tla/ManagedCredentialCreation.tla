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
         recoveryClaimed, unreachableOrphan

vars == <<phase, intentId, material, sourcePublished, childPublished,
          processUp, owner, ownerEpoch, leaseExpired, liveWriterActive,
          admitted, duplicateSeen, conflictRejected, staleRejected,
          recoveryClaimed, unreachableOrphan>>

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
        /\ unreachableOrphan = FALSE

Begin(id) ==
    /\ processUp /\ phase = "None" /\ id \in IntentIds
    /\ phase' = "Writing" /\ intentId' = id /\ owner' = "Live"
    /\ ownerEpoch' = 1 /\ leaseExpired' = FALSE
    /\ liveWriterActive' = TRUE
    /\ UNCHANGED <<material, sourcePublished, childPublished, processUp,
                    admitted, duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed, unreachableOrphan>>

DuplicateBegin(id) ==
    /\ id = intentId /\ phase # "None"
    /\ duplicateSeen' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

ConflictingBegin(id) ==
    /\ id \in IntentIds /\ intentId \in IntentIds /\ id # intentId
    /\ phase # "None"
    /\ conflictRejected' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    staleRejected, recoveryClaimed, unreachableOrphan>>

OriginalWriter(epoch) ==
    /\ epoch = 1 /\ owner = "Live" /\ ownerEpoch = 1
    /\ ~recoveryClaimed

\* Cause/effect table for material-writing creation:
\* R1 original Live epoch 1 + active => Put/MarkReady may advance;
\* R2 expired Writing (None/Partial/Complete) + successful claim => abort only;
\* R3 old epoch-1 external write after claim => unreachable orphan only;
\* R4 only original epoch-1 Ready => Commit/Reject may decide the pair.

PutPartial(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material = "None"
    /\ OriginalWriter(epoch) /\ liveWriterActive
    /\ material' = "Partial"
    /\ UNCHANGED <<phase, intentId, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

PutComplete(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material \in {"None", "Partial"}
    /\ OriginalWriter(epoch) /\ liveWriterActive
    /\ material' = "Complete"
    /\ UNCHANGED <<phase, intentId, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

MarkReady(epoch) ==
    /\ processUp /\ phase = "Writing" /\ material = "Complete"
    /\ OriginalWriter(epoch) /\ liveWriterActive
    /\ phase' = "Ready" /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<intentId, material, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed, unreachableOrphan>>

AbortClaimedWriting(epoch) ==
    /\ processUp /\ phase = "Writing"
    /\ material \in {"None", "Partial", "Complete"}
    /\ epoch = ownerEpoch /\ owner = "Recovery" /\ recoveryClaimed
    /\ phase' = "ReclaimingAbort"
    /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<intentId, material, sourcePublished, childPublished,
                    processUp, owner, ownerEpoch, leaseExpired, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

Commit(epoch) ==
    /\ processUp /\ phase = "Ready" /\ material = "Complete"
    /\ OriginalWriter(epoch)
    /\ admitted = TRUE
    /\ phase' = "Published" /\ sourcePublished' = TRUE
    /\ childPublished' = TRUE /\ owner' = "None"
    /\ UNCHANGED <<intentId, material, processUp, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

Reject(epoch) ==
    /\ processUp /\ phase = "Ready" /\ material = "Complete"
    /\ OriginalWriter(epoch)
    /\ admitted = FALSE
    /\ phase' = "ReclaimingAbort" /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<intentId, material, sourcePublished, childPublished, processUp, owner,
                    ownerEpoch, leaseExpired, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed, unreachableOrphan>>

\* External deletion is retryable because ReclaimingAbort remains durable
\* across every crash prefix. Only exact cleanup completion retires the fact.
CompleteAbortCleanup ==
    /\ processUp /\ phase = "ReclaimingAbort"
    /\ phase' = "Aborted" /\ material' = "None" /\ owner' = "None"
    /\ UNCHANGED <<intentId, sourcePublished, childPublished, processUp,
                    ownerEpoch, leaseExpired, liveWriterActive, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed, unreachableOrphan>>

CancelLiveWriter ==
    /\ phase = "Writing" /\ owner = "Live" /\ liveWriterActive
    /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, admitted, duplicateSeen, conflictRejected,
                    staleRejected, recoveryClaimed, unreachableOrphan>>

\* Abstract deadline passage. Before this action the periodic reconciler has no
\* takeover transition, exactly matching deadline > injected-now in Rust.
ExpireLiveLease ==
    /\ phase = "Writing" /\ owner = "Live" /\ ~leaseExpired
    /\ leaseExpired' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

ClaimExpiredWriting ==
    /\ processUp /\ phase = "Writing" /\ leaseExpired
    /\ ownerEpoch < MaxOwnerEpoch
    /\ owner' = "Recovery" /\ ownerEpoch' = ownerEpoch + 1
    /\ leaseExpired' = FALSE /\ liveWriterActive' = FALSE
    /\ recoveryClaimed' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, admitted, duplicateSeen,
                    conflictRejected, staleRejected, unreachableOrphan>>

\* SecretStore cannot reject an already-running old write after repository
\* takeover. Its attempt-specific physical ref can become only an unreachable
\* orphan; it never restores durable material readiness or pair publishability.
StaleWrite(epoch) ==
    /\ processUp /\ recoveryClaimed /\ ~unreachableOrphan
    /\ phase \in {"Writing", "ReclaimingAbort", "Aborted"}
    /\ epoch = 1 /\ epoch # ownerEpoch
    /\ unreachableOrphan' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed>>

\* MarkReady/Commit from that old epoch are exact-CAS rejections.
StaleCommit(epoch) ==
    /\ recoveryClaimed
    /\ phase \in {"Writing", "ReclaimingAbort", "Aborted"}
    /\ epoch = 1 /\ epoch # ownerEpoch
    /\ staleRejected' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, processUp, owner, ownerEpoch,
                    leaseExpired, liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, recoveryClaimed, unreachableOrphan>>

Crash ==
    /\ processUp /\ phase \in {"Writing", "Ready", "ReclaimingAbort"}
    /\ processUp' = FALSE /\ liveWriterActive' = FALSE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, owner, ownerEpoch, leaseExpired, admitted,
                    duplicateSeen, conflictRejected, staleRejected,
                    recoveryClaimed, unreachableOrphan>>

Restart ==
    /\ ~processUp /\ processUp' = TRUE
    /\ UNCHANGED <<phase, intentId, material, sourcePublished,
                    childPublished, owner, ownerEpoch, leaseExpired,
                    liveWriterActive, admitted, duplicateSeen,
                    conflictRejected, staleRejected, recoveryClaimed,
                    unreachableOrphan>>

Quiesce == /\ phase \in TerminalPhases /\ UNCHANGED vars

PutPartialAny == \E epoch \in 0..MaxOwnerEpoch: PutPartial(epoch)
PutCompleteAny == \E epoch \in 0..MaxOwnerEpoch: PutComplete(epoch)
MarkReadyAny == \E epoch \in 0..MaxOwnerEpoch: MarkReady(epoch)
AbortClaimedWritingAny == \E epoch \in 0..MaxOwnerEpoch: AbortClaimedWriting(epoch)
CommitAny == \E epoch \in 0..MaxOwnerEpoch: Commit(epoch)
RejectAny == \E epoch \in 0..MaxOwnerEpoch: Reject(epoch)
StaleWriteAny == \E epoch \in 0..MaxOwnerEpoch: StaleWrite(epoch)
StaleCommitAny == \E epoch \in 0..MaxOwnerEpoch: StaleCommit(epoch)

Next == (\E id \in IntentIds: Begin(id) \/ DuplicateBegin(id) \/ ConflictingBegin(id))
        \/ PutPartialAny \/ PutCompleteAny \/ MarkReadyAny
        \/ AbortClaimedWritingAny \/ CommitAny \/ RejectAny
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
          /\ unreachableOrphan \in BOOLEAN

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
ReadyBelongsToOriginalWriter ==
    phase = "Ready" => owner = "Live" /\ ownerEpoch = 1 /\ ~recoveryClaimed
PublishedBelongsToOriginalEpoch ==
    phase = "Published" => ownerEpoch = 1 /\ ~recoveryClaimed
RecoveryClaimIsAbortOnly ==
    recoveryClaimed =>
        /\ phase \in {"Writing", "ReclaimingAbort", "Aborted"}
        /\ ~sourcePublished /\ ~childPublished
LateStaleWriteIsOnlyAnUnreachableOrphan ==
    unreachableOrphan => recoveryClaimed /\ ~sourcePublished /\ ~childPublished

CreationSafety ==
    /\ TypeOK /\ SourceAndChildPublishAtomically
    /\ PublishedPairHasCompleteMaterial /\ AbortedCreationIsClean
    /\ UnpublishedMaterialCleanupHasDurableFact
    /\ WritingAlwaysHasDurableOwner /\ RecoveryClaimChangesTheFence
    /\ LiveLeaseIsNeverStolen /\ IntentIdentityNeverDisappears
    /\ ReadyBelongsToOriginalWriter /\ PublishedBelongsToOriginalEpoch
    /\ RecoveryClaimIsAbortOnly
    /\ LateStaleWriteIsOnlyAnUnreachableOrphan

Spec == Init /\ [][Next]_vars

FairSpec ==
    /\ Spec
    /\ WF_vars(Restart)
    /\ SF_vars(ExpireLiveLease)
    /\ SF_vars(ClaimExpiredWriting)
    /\ SF_vars(AbortClaimedWritingAny)
    /\ SF_vars(CompleteAbortCleanup)
    /\ SF_vars(MarkReadyAny)
    /\ SF_vars(CommitAny)
    /\ SF_vars(RejectAny)

AbandonedWritingEventuallyAborts ==
    (phase = "Writing" /\ ~liveWriterActive) ~> phase = "Aborted"
======================================================================
