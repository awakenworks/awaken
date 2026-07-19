----------------------- MODULE CredentialCreation -----------------------
EXTENDS TLC

VARIABLE intent, secret, row, processUp

vars == <<intent, secret, row, processUp>>

Init == /\ intent = FALSE /\ secret = FALSE /\ row = FALSE
        /\ processUp = TRUE

BeginIntent == /\ processUp /\ ~intent /\ ~row
               /\ intent' = TRUE
               /\ UNCHANGED <<secret, row, processUp>>

WriteSecret == /\ processUp /\ intent /\ ~secret /\ ~row
               /\ secret' = TRUE
               /\ UNCHANGED <<intent, row, processUp>>

\* The source publication and intent retirement share one repository transaction.
CommitSourceAndRetireIntent ==
    /\ processUp /\ intent /\ secret /\ ~row
    /\ row' = TRUE /\ intent' = FALSE
    /\ UNCHANGED <<secret, processUp>>

\* Startup recovery compensates every unpublished tracked secret.
RecoverPending == /\ processUp /\ intent /\ ~row
                  /\ intent' = FALSE /\ secret' = FALSE
                  /\ UNCHANGED <<row, processUp>>

Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<intent, secret, row>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<intent, secret, row>>

Next == BeginIntent \/ WriteSecret \/ CommitSourceAndRetireIntent \/
        RecoverPending \/ Crash \/ Restart

TypeOK == /\ intent \in BOOLEAN /\ secret \in BOOLEAN /\ row \in BOOLEAN
          /\ processUp \in BOOLEAN
VisibleRowAlwaysHasMaterial == row => (secret /\ ~intent)
MaterialIsNeverUntracked == secret => (intent \/ row)
RetiredUnpublishedIntentHasNoMaterial == (~intent /\ ~row) => ~secret

Spec == Init /\ [][Next]_vars
=============================================================================
