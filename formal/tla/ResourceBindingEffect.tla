----------------------- MODULE ResourceBindingEffect -----------------------
EXTENDS TLC

CONSTANT StableAgentId

VARIABLE auditState, configCommitted, effectQueued, resourceApplied,
         agentId, processUp

vars == <<auditState, configCommitted, effectQueued, resourceApplied,
          agentId, processUp>>

Init == /\ auditState = "none" /\ configCommitted = FALSE
        /\ effectQueued = FALSE /\ resourceApplied = FALSE
        /\ agentId = StableAgentId /\ processUp = TRUE

RecordAuditIntent ==
    /\ processUp /\ auditState = "none"
    /\ auditState' = "pending"
    /\ UNCHANGED <<configCommitted, effectQueued, resourceApplied, agentId, processUp>>

\* Draft, audit completion, and external-effect journal share one transaction.
CommitDraftAuditAndEffect ==
    /\ processUp /\ auditState = "pending" /\ ~configCommitted
    /\ auditState' = "committed" /\ configCommitted' = TRUE
    /\ effectQueued' = TRUE
    /\ UNCHANGED <<resourceApplied, agentId, processUp>>

\* The separate resource-store upsert is idempotent. A crash before retirement
\* leaves both the applied value and durable effect, so recovery may repeat it.
ApplyEffect ==
    /\ processUp /\ effectQueued
    /\ resourceApplied' = TRUE
    /\ UNCHANGED <<auditState, configCommitted, effectQueued, agentId, processUp>>

ApplyFailure == /\ processUp /\ effectQueued /\ UNCHANGED vars

RetireEffect ==
    /\ processUp /\ effectQueued /\ resourceApplied
    /\ effectQueued' = FALSE
    /\ UNCHANGED <<auditState, configCommitted, resourceApplied, agentId, processUp>>

ReplayCommittedCall ==
    /\ processUp /\ auditState = "committed"
    /\ UNCHANGED vars

Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<auditState, configCommitted, effectQueued,
                        resourceApplied, agentId>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<auditState, configCommitted, effectQueued,
                          resourceApplied, agentId>>

Next == RecordAuditIntent \/ CommitDraftAuditAndEffect \/ ApplyEffect \/
        ApplyFailure \/ RetireEffect \/ ReplayCommittedCall \/ Crash \/ Restart

TypeOK == /\ auditState \in {"none", "pending", "committed"}
          /\ configCommitted \in BOOLEAN /\ effectQueued \in BOOLEAN
          /\ resourceApplied \in BOOLEAN /\ agentId = StableAgentId
          /\ processUp \in BOOLEAN
CommittedDraftHasAuditAndRecoverableBinding ==
    configCommitted => (auditState = "committed" /\ (effectQueued \/ resourceApplied))
RetiredEffectImpliesResourceApplied ==
    (configCommitted /\ ~effectQueued) => resourceApplied
ResourceApplicationRequiresCommittedDraft == resourceApplied => configCommitted
StableBindingIdentity == agentId = StableAgentId

Spec == Init /\ [][Next]_vars
=============================================================================
