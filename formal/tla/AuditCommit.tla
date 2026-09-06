--------------------------- MODULE AuditCommit ---------------------------
EXTENDS TLC, Naturals

CONSTANT Scopes, Summaries
\* These collide under the removed tool:call_id encoding. SQL selection must
\* preserve both coordinates and scope, not infer identity from concatenation.
Identities == {<<"http:POST", "a:b">>, <<"http", "POST:a:b">>}
Keys == {<<s, i[1], i[2]>> : s \in Scopes, i \in Identities}

VARIABLE auditState, auditSummary, businessWrites, committedSummary, processUp
vars == <<auditState, auditSummary, businessWrites, committedSummary, processUp>>

Init == /\ auditState = [k \in Keys |-> "none"]
        /\ auditSummary = [k \in Keys |-> "absent"]
        /\ businessWrites = [k \in Keys |-> 0]
        /\ committedSummary = [k \in Keys |-> "absent"]
        /\ processUp = TRUE

\* A0 absent -> pending. Repeated begin cannot overwrite a recorded summary.
RecordAuditIntent(k, summary) ==
    /\ processUp /\ auditState[k] = "none"
    /\ auditState' = [auditState EXCEPT ![k] = "pending"]
    /\ auditSummary' = [auditSummary EXCEPT ![k] = summary]
    /\ UNCHANGED <<businessWrites, committedSummary, processUp>>

\* A1 mismatched intent -> reject; A2 exact pending -> atomic business+audit.
\* Row locks/SQL transactions are assumed here, not a Rust/SQL refinement.
CommitBusinessAndAudit(k, summary) ==
    /\ processUp /\ auditState[k] = "pending" /\ auditSummary[k] = summary
    /\ auditState' = [auditState EXCEPT ![k] = "committed"]
    /\ businessWrites' = [businessWrites EXCEPT ![k] = @ + 1]
    /\ committedSummary' = [committedSummary EXCEPT ![k] = summary]
    /\ UNCHANGED <<auditSummary, processUp>>

\* A3 committed -> replay; A0 missing and A1 conflicting requests reject.
RetryOrReject(k, summary) ==
    /\ processUp
    /\ (auditState[k] # "pending" \/ auditSummary[k] # summary)
    /\ UNCHANGED vars
Crash == /\ processUp /\ processUp' = FALSE
         /\ UNCHANGED <<auditState, auditSummary, businessWrites, committedSummary>>
Restart == /\ ~processUp /\ processUp' = TRUE
           /\ UNCHANGED <<auditState, auditSummary, businessWrites, committedSummary>>

Next == (\E k \in Keys, summary \in Summaries:
            RecordAuditIntent(k, summary) \/ CommitBusinessAndAudit(k, summary)
            \/ RetryOrReject(k, summary)) \/ Crash \/ Restart

TypeOK == /\ auditState \in [Keys -> {"none", "pending", "committed"}]
          /\ auditSummary \in [Keys -> Summaries \cup {"absent"}]
          /\ businessWrites \in [Keys -> Nat]
          /\ committedSummary \in [Keys -> Summaries \cup {"absent"}]
          /\ processUp \in BOOLEAN
BusinessCommitIffAuditCommitted == \A k \in Keys:
    (businessWrites[k] = 1) <=> (auditState[k] = "committed")
PendingAuditHasNoBusinessCommit == \A k \in Keys:
    auditState[k] # "committed" => businessWrites[k] = 0
ExactIntentOwnsBusinessWrite == \A k \in Keys:
    businessWrites[k] > 0 => committedSummary[k] = auditSummary[k]
CommittedReplayIsSingleWrite == \A k \in Keys: businessWrites[k] <= 1

Spec == Init /\ [][Next]_vars
=============================================================================
