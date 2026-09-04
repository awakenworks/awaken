-------------------------- MODULE CommitEpochGuard --------------------------
EXTENDS TLC

\* Claim-fenced commit/release protocol. The production PostgreSQL guard owns
\* a row-locking transaction, so a successful inner Thread commit is not the
\* observable boundary: the exact guard must first complete release. This model
\* keeps the physical database and async executor in the TCB and proves the
\* repository-owned ordering/failure protocol around that boundary.

VARIABLES phase, innerOutcome, releaseOutcome, returnedOutcome

vars == <<phase, innerOutcome, releaseOutcome, returnedOutcome>>
NoOutcome == "none"

Init ==
    /\ phase = "Held"
    /\ innerOutcome = NoOutcome
    /\ releaseOutcome = NoOutcome
    /\ returnedOutcome = NoOutcome

SettleInner(outcome) ==
    /\ phase = "Held"
    /\ outcome \in {"committed", "rejected"}
    /\ phase' = "InnerSettled"
    /\ innerOutcome' = outcome
    /\ UNCHANGED <<releaseOutcome, returnedOutcome>>

Release(outcome) ==
    /\ phase = "InnerSettled"
    /\ outcome \in {"released", "release_failed"}
    /\ phase' = "Released"
    /\ releaseOutcome' = outcome
    /\ UNCHANGED <<innerOutcome, returnedOutcome>>

Return ==
    /\ phase = "Released"
    /\ returnedOutcome = NoOutcome
    /\ returnedOutcome' =
         IF releaseOutcome = "release_failed" THEN "failed" ELSE innerOutcome
    /\ phase' = "Returned"
    /\ UNCHANGED <<innerOutcome, releaseOutcome>>

\* An attempted early return is observable as a stutter, never as completion.
AttemptEarlyReturn ==
    /\ phase \in {"Held", "InnerSettled"}
    /\ UNCHANGED vars

Next ==
    \/ \E outcome \in {"committed", "rejected"}: SettleInner(outcome)
    \/ \E outcome \in {"released", "release_failed"}: Release(outcome)
    \/ Return
    \/ AttemptEarlyReturn

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"Held", "InnerSettled", "Released", "Returned"}
    /\ innerOutcome \in {NoOutcome, "committed", "rejected"}
    /\ releaseOutcome \in {NoOutcome, "released", "release_failed"}
    /\ returnedOutcome \in {NoOutcome, "committed", "rejected", "failed"}

ReturnRequiresSettledInnerAndRelease ==
    returnedOutcome # NoOutcome =>
        /\ innerOutcome # NoOutcome
        /\ releaseOutcome # NoOutcome
        /\ phase = "Returned"

ReleaseFailureNeverReturnsSuccess ==
    releaseOutcome = "release_failed" =>
        returnedOutcome \notin {"committed", "rejected"}

CommittedSuccessRequiresObservableRelease ==
    returnedOutcome = "committed" => releaseOutcome = "released"

Safety ==
    /\ TypeOK
    /\ ReturnRequiresSettledInnerAndRelease
    /\ ReleaseFailureNeverReturnsSuccess
    /\ CommittedSuccessRequiresObservableRelease

=============================================================================
