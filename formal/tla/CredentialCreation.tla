----------------------- MODULE CredentialCreation -----------------------
EXTENDS TLC

VARIABLE intent, secret, row, failed

vars == <<intent, secret, row, failed>>

Init == /\ intent = FALSE /\ secret = FALSE /\ row = FALSE
        /\ failed = FALSE

Begin == /\ ~intent
         /\ intent' = TRUE
         /\ UNCHANGED <<secret, row, failed>>

WriteSecret == /\ intent /\ ~secret /\ ~failed
               /\ secret' = TRUE
               /\ UNCHANGED <<intent, row, failed>>

CommitRow == /\ secret /\ ~row /\ ~failed
             /\ row' = TRUE
             /\ UNCHANGED <<intent, secret, failed>>

FailAndCompensate == /\ intent /\ ~row /\ ~failed
                     /\ failed' = TRUE /\ secret' = FALSE
                     /\ UNCHANGED <<intent, row>>

Next == Begin \/ WriteSecret \/ CommitRow \/ FailAndCompensate

TypeOK == /\ intent \in BOOLEAN /\ secret \in BOOLEAN /\ row \in BOOLEAN
          /\ failed \in BOOLEAN
VisibleRowAlwaysHasMaterial == row => secret
FailedCreationLeavesNoMaterial == failed => (~row /\ ~secret)

Spec == Init /\ [][Next]_vars
=============================================================================
