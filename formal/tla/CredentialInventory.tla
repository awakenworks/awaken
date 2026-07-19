------------------------ MODULE CredentialInventory ------------------------
EXTENDS TLC

VARIABLE committedSecret, pendingSecret, orphanSecret, foreignSecret,
         processUp, scanned

vars == <<committedSecret, pendingSecret, orphanSecret, foreignSecret,
          processUp, scanned>>

Init == /\ committedSecret = TRUE /\ pendingSecret = TRUE
        /\ orphanSecret = TRUE /\ foreignSecret = TRUE
        /\ processUp = TRUE /\ scanned = FALSE

\* Inventory - committed references - pending-intent references. The prefix
\* fence admits only credential-owned orphan candidates for deletion.
ReconcileInventory ==
    /\ processUp
    /\ orphanSecret' = FALSE /\ scanned' = TRUE
    /\ UNCHANGED <<committedSecret, pendingSecret, foreignSecret, processUp>>

Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<committedSecret, pendingSecret, orphanSecret,
                        foreignSecret, scanned>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<committedSecret, pendingSecret, orphanSecret,
                          foreignSecret, scanned>>

Next == ReconcileInventory \/ Crash \/ Restart

TypeOK == /\ committedSecret \in BOOLEAN /\ pendingSecret \in BOOLEAN
          /\ orphanSecret \in BOOLEAN /\ foreignSecret \in BOOLEAN
          /\ processUp \in BOOLEAN /\ scanned \in BOOLEAN
CommittedMaterialIsNeverDeleted == committedSecret
PendingIntentMaterialIsNeverDeleted == pendingSecret
ForeignNamespaceIsNeverDeleted == foreignSecret
CompletedScanLeavesNoCredentialOrphan == scanned => ~orphanSecret

Spec == Init /\ [][Next]_vars
=============================================================================
