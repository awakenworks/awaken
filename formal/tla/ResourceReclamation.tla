------------------------ MODULE ResourceReclamation ------------------------
EXTENDS Naturals

CONSTANTS MaxReferences, MaxLeases, MaxGeneration

VARIABLES lifecycle, intentState, claimGeneration, fence, fenceGeneration,
          references, leases, physicalPresent, purgeObserved, purgeGeneration,
          lastPurgeReferences, lastPurgeLeases, lastPurgeFenceGeneration,
          receipt, completedGeneration, processUp, lastReferenceAttempt,
          lastReferenceCount, lastLeaseAttempt, lastLeaseCount,
          lastReceiptAttempt, lastReceiptValue

vars == <<lifecycle, intentState, claimGeneration, fence, fenceGeneration,
          references, leases, physicalPresent, purgeObserved, purgeGeneration,
          lastPurgeReferences, lastPurgeLeases, lastPurgeFenceGeneration,
          receipt, completedGeneration, processUp, lastReferenceAttempt,
          lastReferenceCount, lastLeaseAttempt, lastLeaseCount,
          lastReceiptAttempt, lastReceiptValue>>

Init == /\ lifecycle = "active"
        /\ intentState = "none"
        /\ claimGeneration = 0
        /\ fence = FALSE
        /\ fenceGeneration = 0
        /\ references = 0
        /\ leases = 0
        /\ physicalPresent = TRUE
        /\ purgeObserved = FALSE
        /\ purgeGeneration = 0
        /\ lastPurgeReferences = 0
        /\ lastPurgeLeases = 0
        /\ lastPurgeFenceGeneration = 0
        /\ receipt = FALSE
        /\ completedGeneration = 0
        /\ processUp = TRUE
        /\ lastReferenceAttempt = "none"
        /\ lastReferenceCount = 0
        /\ lastLeaseAttempt = "none"
        /\ lastLeaseCount = 0
        /\ lastReceiptAttempt = "none"
        /\ lastReceiptValue = FALSE

\* Reference creation and fence acquisition serialize on physical identity.
\* A shared immutable File may recreate the physical blob after an older
\* Workspace tombstone completed, so a receipt proves a safe purge operation,
\* not permanent global absence.
TryAddReference ==
    /\ references < MaxReferences
    /\ lastReferenceCount' = references
    /\ IF fence
          THEN /\ references' = references
               /\ physicalPresent' = physicalPresent
               /\ lastReferenceAttempt' = "rejected"
          ELSE /\ references' = references + 1
               /\ physicalPresent' = TRUE
               /\ lastReferenceAttempt' = "added"
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, leases, purgeObserved, purgeGeneration,
                    lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastLeaseAttempt, lastLeaseCount,
                    lastReceiptAttempt, lastReceiptValue>>

RemoveReference ==
    /\ references > 0
    /\ references' = references - 1
    /\ lastReferenceAttempt' = "none"
    /\ lastReferenceCount' = references
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, leases, physicalPresent, purgeObserved,
                    purgeGeneration, lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastLeaseAttempt, lastLeaseCount,
                    lastReceiptAttempt, lastReceiptValue>>

TryAcquireLease ==
    /\ leases < MaxLeases
    /\ lastLeaseCount' = leases
    /\ IF lifecycle = "active" /\ ~fence
          THEN /\ leases' = leases + 1
               /\ lastLeaseAttempt' = "acquired"
          ELSE /\ leases' = leases
               /\ lastLeaseAttempt' = "rejected"
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, references, physicalPresent,
                    purgeObserved, purgeGeneration, lastPurgeReferences,
                    lastPurgeLeases, lastPurgeFenceGeneration, receipt,
                    completedGeneration, processUp, lastReferenceAttempt,
                    lastReferenceCount, lastReceiptAttempt, lastReceiptValue>>

ReleaseLease ==
    /\ leases > 0
    /\ leases' = leases - 1
    /\ lastLeaseAttempt' = "none"
    /\ lastLeaseCount' = leases
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, references, physicalPresent,
                    purgeObserved, purgeGeneration, lastPurgeReferences,
                    lastPurgeLeases, lastPurgeFenceGeneration, receipt,
                    completedGeneration, processUp, lastReferenceAttempt,
                    lastReferenceCount, lastReceiptAttempt, lastReceiptValue>>

LogicalDelete ==
    /\ lifecycle = "active"
    /\ intentState = "none"
    /\ lifecycle' = "deleted"
    /\ intentState' = "pending"
    /\ UNCHANGED <<claimGeneration, fence, fenceGeneration, references, leases,
                    physicalPresent, purgeObserved, purgeGeneration,
                    lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastReferenceAttempt, lastReferenceCount,
                    lastLeaseAttempt, lastLeaseCount, lastReceiptAttempt,
                    lastReceiptValue>>

Claim ==
    /\ processUp
    /\ intentState = "pending"
    /\ claimGeneration < MaxGeneration
    /\ intentState' = "claimed"
    /\ claimGeneration' = claimGeneration + 1
    /\ purgeObserved' = FALSE
    /\ purgeGeneration' = 0
    /\ UNCHANGED <<lifecycle, fence, fenceGeneration, references, leases,
                    physicalPresent, lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastReferenceAttempt, lastReferenceCount,
                    lastLeaseAttempt, lastLeaseCount, lastReceiptAttempt,
                    lastReceiptValue>>

Defer ==
    /\ processUp
    /\ intentState = "claimed"
    /\ ~fence
    /\ references > 0 \/ leases > 0
    /\ intentState' = "pending"
    /\ UNCHANGED <<lifecycle, claimGeneration, fence, fenceGeneration,
                    references, leases, physicalPresent, purgeObserved,
                    purgeGeneration, lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastReferenceAttempt, lastReferenceCount,
                    lastLeaseAttempt, lastLeaseCount, lastReceiptAttempt,
                    lastReceiptValue>>

AcquireFence ==
    /\ processUp
    /\ intentState = "claimed"
    /\ references = 0
    /\ leases = 0
    /\ (~fence \/ fenceGeneration < claimGeneration)
    /\ fence' = TRUE
    /\ fenceGeneration' = claimGeneration
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, references, leases,
                    physicalPresent, purgeObserved, purgeGeneration,
                    lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastReferenceAttempt, lastReferenceCount,
                    lastLeaseAttempt, lastLeaseCount, lastReceiptAttempt,
                    lastReceiptValue>>

Purge ==
    /\ processUp
    /\ intentState = "claimed"
    /\ fence
    /\ fenceGeneration = claimGeneration
    /\ references = 0
    /\ leases = 0
    /\ physicalPresent' = FALSE
    /\ purgeObserved' = TRUE
    /\ purgeGeneration' = claimGeneration
    /\ lastPurgeReferences' = references
    /\ lastPurgeLeases' = leases
    /\ lastPurgeFenceGeneration' = fenceGeneration
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, references, leases, receipt,
                    completedGeneration, processUp, lastReferenceAttempt,
                    lastReferenceCount, lastLeaseAttempt, lastLeaseCount,
                    lastReceiptAttempt, lastReceiptValue>>

ReleaseFence ==
    /\ processUp
    /\ intentState = "claimed"
    /\ fence
    /\ fenceGeneration = claimGeneration
    /\ purgeObserved
    /\ purgeGeneration = claimGeneration
    /\ fence' = FALSE
    /\ fenceGeneration' = 0
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, references, leases,
                    physicalPresent, purgeObserved, purgeGeneration,
                    lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, receipt, completedGeneration,
                    processUp, lastReferenceAttempt, lastReferenceCount,
                    lastLeaseAttempt, lastLeaseCount, lastReceiptAttempt,
                    lastReceiptValue>>

Complete ==
    /\ processUp
    /\ intentState = "claimed"
    /\ ~fence
    /\ purgeObserved
    /\ purgeGeneration = claimGeneration
    /\ intentState' = "completed"
    /\ receipt' = TRUE
    /\ completedGeneration' = claimGeneration
    /\ lastReceiptAttempt' = "current"
    /\ lastReceiptValue' = receipt
    /\ UNCHANGED <<lifecycle, claimGeneration, fence, fenceGeneration,
                    references, leases, physicalPresent, purgeObserved,
                    purgeGeneration, lastPurgeReferences, lastPurgeLeases,
                    lastPurgeFenceGeneration, processUp,
                    lastReferenceAttempt, lastReferenceCount,
                    lastLeaseAttempt, lastLeaseCount>>

Crash ==
    /\ processUp
    /\ processUp' = FALSE
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, references, leases, physicalPresent,
                    purgeObserved, purgeGeneration, lastPurgeReferences,
                    lastPurgeLeases, lastPurgeFenceGeneration, receipt,
                    completedGeneration, lastReferenceAttempt,
                    lastReferenceCount, lastLeaseAttempt, lastLeaseCount,
                    lastReceiptAttempt, lastReceiptValue>>

Restart ==
    /\ ~processUp
    /\ processUp' = TRUE
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, references, leases, physicalPresent,
                    purgeObserved, purgeGeneration, lastPurgeReferences,
                    lastPurgeLeases, lastPurgeFenceGeneration, receipt,
                    completedGeneration, lastReferenceAttempt,
                    lastReferenceCount, lastLeaseAttempt, lastLeaseCount,
                    lastReceiptAttempt, lastReceiptValue>>

\* Lease recovery advances the durable fencing generation. An older physical
\* fence may remain, but it must be adopted before the recovered claim can purge.
RecoverClaim ==
    /\ processUp
    /\ intentState = "claimed"
    /\ claimGeneration < MaxGeneration
    /\ claimGeneration' = claimGeneration + 1
    /\ purgeObserved' = FALSE
    /\ purgeGeneration' = 0
    /\ UNCHANGED <<lifecycle, intentState, fence, fenceGeneration, references,
                    leases, physicalPresent, lastPurgeReferences,
                    lastPurgeLeases, lastPurgeFenceGeneration, receipt,
                    completedGeneration, processUp, lastReferenceAttempt,
                    lastReferenceCount, lastLeaseAttempt, lastLeaseCount,
                    lastReceiptAttempt, lastReceiptValue>>

RejectStaleReceipt ==
    /\ processUp
    /\ intentState = "claimed"
    /\ claimGeneration > 1
    /\ lastReceiptAttempt' = "stale"
    /\ lastReceiptValue' = receipt
    /\ UNCHANGED <<lifecycle, intentState, claimGeneration, fence,
                    fenceGeneration, references, leases, physicalPresent,
                    purgeObserved, purgeGeneration, lastPurgeReferences,
                    lastPurgeLeases, lastPurgeFenceGeneration, receipt,
                    completedGeneration, processUp, lastReferenceAttempt,
                    lastReferenceCount, lastLeaseAttempt, lastLeaseCount>>

Next == TryAddReference \/ RemoveReference \/ TryAcquireLease \/ ReleaseLease \/
        LogicalDelete \/ Claim \/ Defer \/ AcquireFence \/ Purge \/
        ReleaseFence \/ Complete \/ Crash \/ Restart \/ RecoverClaim \/
        RejectStaleReceipt

TypeOK == /\ lifecycle \in {"active", "deleted"}
          /\ intentState \in {"none", "pending", "claimed", "completed"}
          /\ claimGeneration \in 0..MaxGeneration
          /\ fence \in BOOLEAN
          /\ fenceGeneration \in 0..MaxGeneration
          /\ references \in 0..MaxReferences
          /\ leases \in 0..MaxLeases
          /\ physicalPresent \in BOOLEAN
          /\ purgeObserved \in BOOLEAN
          /\ purgeGeneration \in 0..MaxGeneration
          /\ lastPurgeReferences \in 0..MaxReferences
          /\ lastPurgeLeases \in 0..MaxLeases
          /\ lastPurgeFenceGeneration \in 0..MaxGeneration
          /\ receipt \in BOOLEAN
          /\ completedGeneration \in 0..MaxGeneration
          /\ processUp \in BOOLEAN
          /\ lastReferenceAttempt \in {"none", "added", "rejected"}
          /\ lastReferenceCount \in 0..MaxReferences
          /\ lastLeaseAttempt \in {"none", "acquired", "rejected"}
          /\ lastLeaseCount \in 0..MaxLeases
          /\ lastReceiptAttempt \in {"none", "current", "stale"}
          /\ lastReceiptValue \in BOOLEAN

IntentRequiresLogicalDelete == intentState # "none" => lifecycle = "deleted"

FenceHasNoReferencesOrLeases == fence => references = 0 /\ leases = 0

FenceCannotLeadTheDurableClaim == fence => fenceGeneration <= claimGeneration

RejectedReferenceDoesNotCreateEdge ==
    lastReferenceAttempt = "rejected" => references = lastReferenceCount

RejectedLeaseDoesNotCreateHandle ==
    lastLeaseAttempt = "rejected" => leases = lastLeaseCount

PurgeWasFencedAndUnreferenced ==
    purgeObserved =>
        /\ purgeGeneration > 0
        /\ lastPurgeReferences = 0
        /\ lastPurgeLeases = 0
        /\ lastPurgeFenceGeneration = purgeGeneration

ReceiptRequiresCurrentSafePurge ==
    receipt =>
        /\ intentState = "completed"
        /\ purgeObserved
        /\ completedGeneration = purgeGeneration
        /\ completedGeneration > 0

StaleReceiptCannotCommit ==
    lastReceiptAttempt = "stale" => receipt = lastReceiptValue

Spec == Init /\ [][Next]_vars
=============================================================================
