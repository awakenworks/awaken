------------------------ MODULE AgentInputRevision ------------------------
EXTENDS Naturals

CONSTANTS MaxRevision, Writers, SerializeAdmissions

\* One authority, exposing the real read/decide/commit boundary. The lock
\* represents the memory mutex, SQLite IMMEDIATE transaction, or PostgreSQL
\* transaction-scoped aggregate advisory lock (including an absent row).
\* C1 concurrent absence reads; C2 same/different content; C3 invalid, successor,
\* stale/exhausted revision; C4 abort; C5 commit response lost. R1 positive exact replay -> no write;
\* R2 exact successor -> one write; R3 reject -> no mutation; R4 abort -> no write.
\* SerializeAdmissions=FALSE is the missing-row-lock regression mutant.
\* This abstract protocol is NOT a proof of SQL/JSON or the async driver.
VARIABLES current, content, attempted, proposed, observed, observedContent,
          phase, owner, writes, before, result, lastAttempt

vars == <<current, content, attempted, proposed, observed, observedContent,
          phase, owner, writes, before, result, lastAttempt>>

Init == /\ current = 0
        /\ content = 0
        /\ attempted = [w \in Writers |-> 0]
        /\ proposed = [w \in Writers |-> 0]
        /\ observed = [w \in Writers |-> 0]
        /\ observedContent = [w \in Writers |-> 0]
        /\ phase = [w \in Writers |-> "idle"]
        /\ owner = 0
        /\ writes = [r \in 1..MaxRevision |-> 0]
        /\ before = 0
        /\ result = "idle"
        /\ lastAttempt = 0

Read(w) ==
    /\ phase[w] = "idle"
    /\ ~SerializeAdmissions \/ owner = 0
    /\ owner' = IF SerializeAdmissions THEN w ELSE owner
    /\ attempted' \in [Writers -> 0..MaxRevision]
    /\ \A other \in Writers \ {w}: attempted'[other] = attempted[other]
    /\ proposed' \in [Writers -> {0, 1}]
    /\ \A other \in Writers \ {w}: proposed'[other] = proposed[other]
    /\ observed' = [observed EXCEPT ![w] = current]
    /\ observedContent' = [observedContent EXCEPT ![w] = content]
    /\ phase' = [phase EXCEPT ![w] = "read"]
    /\ UNCHANGED <<current, content, writes, before, result, lastAttempt>>

IsReplay(w) == /\ attempted[w] > 0
               /\ attempted[w] = observed[w]
               /\ proposed[w] = observedContent[w]
IsSuccessor(w) == /\ attempted[w] = observed[w] + 1
                  /\ attempted[w] <= MaxRevision

Decide(w) ==
    /\ phase[w] = "read"
    /\ phase' = [phase EXCEPT ![w] =
        IF IsReplay(w) THEN "replayed"
        ELSE IF IsSuccessor(w) THEN "applied" ELSE "rejected"]
    /\ UNCHANGED <<current, content, attempted, proposed, observed,
                   observedContent, owner, writes, before, result, lastAttempt>>

Commit(w) ==
    /\ phase[w] \in {"applied", "replayed", "rejected"}
    /\ before' = current
    /\ result' = phase[w]
    /\ lastAttempt' = attempted[w]
    /\ IF phase[w] = "applied"
          THEN /\ current' = attempted[w]
               /\ content' = proposed[w]
               /\ writes' = [writes EXCEPT ![attempted[w]] = @ + 1]
          ELSE UNCHANGED <<current, content, writes>>
    /\ owner' = IF SerializeAdmissions THEN 0 ELSE owner
    /\ phase' = [phase EXCEPT ![w] = "unacknowledged"]
    /\ UNCHANGED <<attempted, proposed, observed, observedContent>>

Abort(w) ==
    /\ phase[w] \in {"read", "applied", "replayed", "rejected"}
    /\ phase' = [phase EXCEPT ![w] = "idle"]
    /\ owner' = IF SerializeAdmissions THEN 0 ELSE owner
    /\ UNCHANGED <<current, content, attempted, proposed, observed,
                   observedContent, writes, before, result, lastAttempt>>

\* Losing an acknowledgement does not undo durable truth. Retry freezes the
\* same intent and re-observes current truth; it must not invent a new revision.
Retry(w) ==
    /\ phase[w] = "unacknowledged"
    /\ ~SerializeAdmissions \/ owner = 0
    /\ owner' = IF SerializeAdmissions THEN w ELSE owner
    /\ observed' = [observed EXCEPT ![w] = current]
    /\ observedContent' = [observedContent EXCEPT ![w] = content]
    /\ phase' = [phase EXCEPT ![w] = "read"]
    /\ UNCHANGED <<current, content, attempted, proposed, writes, before, result, lastAttempt>>

Acknowledge(w) ==
    /\ phase[w] = "unacknowledged"
    /\ phase' = [phase EXCEPT ![w] = "idle"]
    /\ UNCHANGED <<current, content, attempted, proposed, observed, observedContent,
                   owner, writes, before, result, lastAttempt>>

Next == \E w \in Writers: Read(w) \/ Decide(w) \/ Commit(w) \/ Abort(w)
                           \/ Retry(w) \/ Acknowledge(w)

TypeOK == /\ current \in 0..MaxRevision
          /\ content \in {0, 1}
          /\ attempted \in [Writers -> 0..MaxRevision]
          /\ proposed \in [Writers -> {0, 1}]
          /\ observed \in [Writers -> 0..MaxRevision]
          /\ observedContent \in [Writers -> {0, 1}]
          /\ phase \in [Writers -> {"idle", "read", "applied", "replayed", "rejected", "unacknowledged"}]
          /\ owner \in Writers \cup {0}
          /\ writes \in [1..MaxRevision -> Nat]
          /\ before \in 0..MaxRevision
          /\ lastAttempt \in 0..MaxRevision
          /\ result \in {"idle", "applied", "replayed", "rejected"}

AppliedIsExactSuccessor == result = "applied" => current = before + 1
ReplayDoesNotAdvance == result = "replayed" => current = before /\ lastAttempt = current
RejectedDoesNotMutate == result = "rejected" => current = before
OneWriterPerRevision == \A r \in 1..MaxRevision: writes[r] <= 1

Spec == Init /\ [][Next]_vars
=============================================================================
