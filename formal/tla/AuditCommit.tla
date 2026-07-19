--------------------------- MODULE AuditCommit ---------------------------
EXTENDS TLC

CONSTANT StableCallId

VARIABLE auditState, businessCommitted, callId, processUp

vars == <<auditState, businessCommitted, callId, processUp>>

Init == /\ auditState = "none" /\ businessCommitted = FALSE
        /\ callId = StableCallId /\ processUp = TRUE

RecordAuditIntent == /\ processUp /\ auditState = "none"
                     /\ auditState' = "pending"
                     /\ UNCHANGED <<businessCommitted, callId, processUp>>

\* The config mutation and pending->committed audit transition are one transaction.
CommitBusinessAndAudit ==
    /\ processUp /\ auditState = "pending" /\ ~businessCommitted
    /\ auditState' = "committed" /\ businessCommitted' = TRUE
    /\ UNCHANGED <<callId, processUp>>

RetryCommittedCall == /\ processUp /\ auditState = "committed"
                      /\ UNCHANGED vars

Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<auditState, businessCommitted, callId>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<auditState, businessCommitted, callId>>

Next == RecordAuditIntent \/ CommitBusinessAndAudit \/ RetryCommittedCall \/ Crash \/ Restart

TypeOK == /\ auditState \in {"none", "pending", "committed"}
          /\ businessCommitted \in BOOLEAN
          /\ callId = StableCallId /\ processUp \in BOOLEAN
BusinessCommitIffAuditCommitted == businessCommitted <=> auditState = "committed"
PendingAuditHasNoBusinessCommit == auditState = "pending" => ~businessCommitted
AuditIdentityIsStable == callId = StableCallId

Spec == Init /\ [][Next]_vars
=============================================================================
