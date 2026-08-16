------------------------ MODULE CredentialInventory ------------------------
EXTENDS FiniteSets, TLC

Secrets == {"committed", "pending", "orphan", "foreign"}
ProtectedReferences == {"committed", "pending", "foreign"}

VARIABLE present, protected, orphanCandidates, reported, processUp, scanned

vars == <<present, protected, orphanCandidates, reported, processUp, scanned>>

Init == /\ present = Secrets
        /\ protected = {} /\ orphanCandidates = {} /\ reported = {}
        /\ processUp = TRUE /\ scanned = FALSE

\* Inventory - committed references - pending-intent references. The generic
\* reconciler records the protected set and reports the remaining candidates.
\* It is deliberately observational: the repository and SecretStore do not
\* share an atomic orphan-claim/delete boundary, so present is unchanged.
ReconcileInventory ==
    /\ processUp
    /\ protected' = ProtectedReferences
    /\ orphanCandidates' = present \ ProtectedReferences
    /\ reported' = present \ ProtectedReferences
    /\ scanned' = TRUE
    /\ UNCHANGED <<present, processUp>>

Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<present, protected, orphanCandidates, reported, scanned>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<present, protected, orphanCandidates, reported, scanned>>

Next == ReconcileInventory \/ Crash \/ Restart

TypeOK == /\ present \subseteq Secrets /\ protected \subseteq Secrets
          /\ orphanCandidates \subseteq Secrets /\ reported \subseteq Secrets
          /\ processUp \in BOOLEAN /\ scanned \in BOOLEAN
CommittedMaterialIsNeverDeleted == "committed" \in present
PendingIntentMaterialIsNeverDeleted == "pending" \in present
ForeignNamespaceIsNeverDeleted == "foreign" \in present
ScanProtectsEveryDurableReference ==
    scanned => protected = ProtectedReferences
OnlyUnprotectedPresentMaterialIsCandidate ==
    scanned => orphanCandidates = present \ protected
EveryOrphanCandidateIsReported == scanned => reported = orphanCandidates
ObservedOrphanIsRetainedWithoutAtomicClaim ==
    "orphan" \in reported => "orphan" \in present

Spec == Init /\ [][Next]_vars
=============================================================================
