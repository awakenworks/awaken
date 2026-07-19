----------------------- MODULE ManagementAuditIntent -----------------------
EXTENDS TLC

CONSTANT StableCallId

VARIABLE auditState, businessCommitted, callId, processUp

vars == <<auditState, businessCommitted, callId, processUp>>

Init == /\ auditState = "none" /\ businessCommitted = FALSE
        /\ callId = StableCallId /\ processUp = TRUE

RecordIntent == /\ processUp /\ auditState = "none"
                /\ auditState' = "pending"
                /\ UNCHANGED <<businessCommitted, callId, processUp>>

\* Generic management stores may not share the config database. The durable
\* audit intent is therefore the admission fence before their business write.
CommitBusiness == /\ processUp /\ auditState = "pending" /\ ~businessCommitted
                  /\ businessCommitted' = TRUE
                  /\ UNCHANGED <<auditState, callId, processUp>>

MarkAuditCommitted == /\ processUp /\ auditState = "pending" /\ businessCommitted
                      /\ auditState' = "committed"
                      /\ UNCHANGED <<businessCommitted, callId, processUp>>

Retry == /\ processUp /\ auditState # "none" /\ UNCHANGED vars
Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<auditState, businessCommitted, callId>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<auditState, businessCommitted, callId>>

Next == RecordIntent \/ CommitBusiness \/ MarkAuditCommitted \/ Retry \/ Crash \/ Restart

TypeOK == /\ auditState \in {"none", "pending", "committed"}
          /\ businessCommitted \in BOOLEAN /\ callId = StableCallId
          /\ processUp \in BOOLEAN
NoBusinessWithoutDurableAudit == businessCommitted => auditState # "none"
CommittedAuditHasBusiness == auditState = "committed" => businessCommitted
StableIdentity == callId = StableCallId

Spec == Init /\ [][Next]_vars
=============================================================================
