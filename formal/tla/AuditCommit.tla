--------------------------- MODULE AuditCommit ---------------------------
EXTENDS TLC

CONSTANT StableCallId

VARIABLE auditRecorded, businessCommitted, callId, processUp

vars == <<auditRecorded, businessCommitted, callId, processUp>>

Init == /\ auditRecorded = FALSE /\ businessCommitted = FALSE
        /\ callId = StableCallId /\ processUp = TRUE

RecordAudit == /\ processUp /\ ~auditRecorded
               /\ auditRecorded' = TRUE
               /\ UNCHANGED <<businessCommitted, callId, processUp>>

CommitBusiness == /\ processUp /\ auditRecorded /\ ~businessCommitted
                  /\ businessCommitted' = TRUE
                  /\ UNCHANGED <<auditRecorded, callId, processUp>>

RetryAudit == /\ processUp /\ auditRecorded
              /\ UNCHANGED vars

Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<auditRecorded, businessCommitted, callId>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<auditRecorded, businessCommitted, callId>>

Next == RecordAudit \/ CommitBusiness \/ RetryAudit \/ Crash \/ Restart

TypeOK == /\ auditRecorded \in BOOLEAN /\ businessCommitted \in BOOLEAN
          /\ callId = StableCallId /\ processUp \in BOOLEAN
CommittedBusinessHasAudit == businessCommitted => auditRecorded
AuditIdentityIsStable == callId = StableCallId

Spec == Init /\ [][Next]_vars
=============================================================================
