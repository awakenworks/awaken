----------------------- MODULE ResourceBindingEffect -----------------------
EXTENDS TLC, Naturals
CONSTANT Workers, MaxRevision
\* One fixed workspace/Agent journal key, successive complete effect identities
\* projected to binding revisions, NOT AgentConfig CAS generations. An exact
\* repeated binding intent is abstract stuttering. Heap equality and SQL locks need
\* production evidence; AgentInputRevision owns the target revision contract.
VARIABLE auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
         resourceRevision, phase, captured, processUp
vars == <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
          resourceRevision, phase, captured, processUp>>

Init == /\ auditPending = FALSE /\ auditedBindingRevision = 0 /\ desiredBindingRevision = 0
        /\ effectQueued = 0 /\ resourceRevision = 0
        /\ phase = [w \in Workers |-> "idle"]
        /\ captured = [w \in Workers |-> 0] /\ processUp = TRUE
RecordAuditIntent ==
    /\ processUp /\ ~auditPending /\ desiredBindingRevision < MaxRevision
    /\ auditPending' = TRUE
    /\ UNCHANGED <<auditedBindingRevision, desiredBindingRevision, effectQueued,
                   resourceRevision, phase, captured, processUp>>
\* The one logical effect slot cannot replace pending work. Draft+audit+journal
\* advance in one transaction; a crash cannot publish only a subset.
CommitDraftAuditAndEffect ==
    /\ processUp /\ auditPending /\ effectQueued = 0
    /\ desiredBindingRevision < MaxRevision /\ desiredBindingRevision' = desiredBindingRevision + 1
    /\ auditedBindingRevision' = desiredBindingRevision' /\ effectQueued' = desiredBindingRevision'
    /\ auditPending' = FALSE
    /\ UNCHANGED <<resourceRevision, phase, captured, processUp>>
ObserveEffect(w) ==
    /\ processUp /\ phase[w] = "idle" /\ effectQueued > 0
    /\ phase' = [phase EXCEPT ![w] = "observed"]
    /\ captured' = [captured EXCEPT ![w] = effectQueued]
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
                   resourceRevision, processUp>>
\* Reuse the existing target's exact replay/successor rule, never blind upsert.
ApplyEffect(w) ==
    /\ processUp /\ phase[w] = "observed"
    /\ captured[w] \in {resourceRevision, resourceRevision + 1}
    /\ resourceRevision' = captured[w]
    /\ phase' = [phase EXCEPT ![w] = "applied"]
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
                   captured, processUp>>
Readback(w) ==
    /\ processUp /\ phase[w] = "applied" /\ resourceRevision = captured[w]
    /\ phase' = [phase EXCEPT ![w] = "readback"]
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
                   resourceRevision, captured, processUp>>
\* ABA: two workers may hold a receipt for old work while a successor is queued.
\* Retirement compares the current complete identity inside its atomic fence.
RetireEffect(w) ==
    /\ processUp /\ phase[w] = "readback"
    /\ effectQueued' = IF effectQueued = captured[w] THEN 0 ELSE effectQueued
    /\ phase' = [phase EXCEPT ![w] = "idle"]
    /\ captured' = [captured EXCEPT ![w] = 0]
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, resourceRevision, processUp>>
RetryAfterFailure(w) ==
    /\ processUp /\ phase[w] # "idle"
    /\ phase' = [phase EXCEPT ![w] = "idle"]
    /\ captured' = [captured EXCEPT ![w] = 0]
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
                   resourceRevision, processUp>>
Crash ==
    /\ processUp /\ processUp' = FALSE
    /\ phase' = [w \in Workers |-> "idle"] /\ captured' = [w \in Workers |-> 0]
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued, resourceRevision>>
Restart ==
    /\ ~processUp /\ processUp' = TRUE
    /\ UNCHANGED <<auditPending, auditedBindingRevision, desiredBindingRevision, effectQueued,
                   resourceRevision, phase, captured>>
Next == RecordAuditIntent \/ CommitDraftAuditAndEffect \/ Crash \/ Restart
        \/ (\E w \in Workers: ObserveEffect(w) \/ ApplyEffect(w) \/ Readback(w)
                              \/ RetireEffect(w) \/ RetryAfterFailure(w))

TypeOK == /\ auditPending \in BOOLEAN /\ processUp \in BOOLEAN
          /\ auditedBindingRevision \in 0..MaxRevision /\ desiredBindingRevision \in 0..MaxRevision
          /\ effectQueued \in 0..MaxRevision /\ resourceRevision \in 0..MaxRevision
          /\ phase \in [Workers -> {"idle", "observed", "applied", "readback"}]
          /\ captured \in [Workers -> 0..MaxRevision]
CommittedDraftHasAuditAndRecoverableBinding ==
    /\ desiredBindingRevision = auditedBindingRevision
    /\ desiredBindingRevision > 0 => (effectQueued = desiredBindingRevision \/ resourceRevision = desiredBindingRevision)
RetiredEffectImpliesResourceApplied ==
    (desiredBindingRevision > 0 /\ effectQueued = 0) => resourceRevision = desiredBindingRevision
ResourceApplicationRequiresCommittedDraft == resourceRevision <= desiredBindingRevision
ReceiptsReferToCommittedHistory == \A w \in Workers:
    /\ captured[w] <= desiredBindingRevision
    /\ phase[w] = "readback" => (captured[w] > 0 /\ captured[w] <= resourceRevision)

Spec == Init /\ [][Next]_vars
=============================================================================
