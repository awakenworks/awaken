-------------------------- MODULE DeploymentCAS --------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Deployments, Replicas, Occurrences, MaxRevision, ScheduledCapacity

\* The bounded model uses the same model values for Deployment identities and
\* their one representative scheduled occurrence. Repeated real occurrences
\* use distinct durable claim ids but obey the same per-Deployment fencing.
OccurrenceDeployment(o) == o

Statuses == {"active", "paused", "archived"}

VARIABLE present, revision, status, scheduled, observed,
         claims, runs, claimRevision, archived, staleRejected

vars == <<present, revision, status, scheduled, observed,
          claims, runs, claimRevision, archived, staleRejected>>

ScheduledCount(p, sch, st) ==
    Cardinality({d \in Deployments : p[d] /\ sch[d] /\ st[d] # "archived"})

Init ==
    /\ present = [d \in Deployments |-> FALSE]
    /\ revision = [d \in Deployments |-> 0]
    /\ status = [d \in Deployments |-> "active"]
    /\ scheduled = [d \in Deployments |-> FALSE]
    /\ observed = [r \in Replicas |-> [d \in Deployments |-> 0]]
    /\ claims = {}
    /\ runs = {}
    /\ claimRevision = [o \in Occurrences |-> 0]
    /\ archived = {}
    /\ staleRejected = FALSE

Read(r, d) ==
    /\ observed' = [observed EXCEPT ![r][d] = revision[d]]
    /\ UNCHANGED <<present, revision, status, scheduled,
                    claims, runs, claimRevision, archived, staleRejected>>

Create(d, isScheduled) ==
    /\ ~present[d]
    /\ ScheduledCount(
           [present EXCEPT ![d] = TRUE],
           [scheduled EXCEPT ![d] = isScheduled],
           status) <= ScheduledCapacity
    /\ present' = [present EXCEPT ![d] = TRUE]
    /\ revision' = [revision EXCEPT ![d] = 0]
    /\ status' = [status EXCEPT ![d] = "active"]
    /\ scheduled' = [scheduled EXCEPT ![d] = isScheduled]
    /\ UNCHANGED <<observed, claims, runs, claimRevision, archived, staleRejected>>

Write(r, d, nextStatus, nextScheduled) ==
    /\ present[d]
    /\ status[d] # "archived"
    /\ observed[r][d] = revision[d]
    /\ revision[d] < MaxRevision
    /\ nextStatus \in Statuses
    /\ ScheduledCount(
           present,
           [scheduled EXCEPT ![d] = nextScheduled],
           [status EXCEPT ![d] = nextStatus]) <= ScheduledCapacity
    /\ revision' = [revision EXCEPT ![d] = @ + 1]
    /\ status' = [status EXCEPT ![d] = nextStatus]
    /\ scheduled' = [scheduled EXCEPT ![d] = nextScheduled]
    /\ archived' = IF nextStatus = "archived" THEN archived \cup {d} ELSE archived
    /\ UNCHANGED <<present, observed, claims, runs, claimRevision, staleRejected>>

StaleWrite(r, d) ==
    /\ present[d]
    /\ observed[r][d] # revision[d]
    /\ staleRejected' = TRUE
    /\ UNCHANGED <<present, revision, status, scheduled, observed,
                    claims, runs, claimRevision, archived>>

Claim(r, o) ==
    LET d == OccurrenceDeployment(o) IN
    /\ present[d]
    /\ status[d] = "active"
    /\ scheduled[d]
    /\ o \notin claims
    /\ observed[r][d] = revision[d]
    /\ revision[d] < MaxRevision
    /\ revision' = [revision EXCEPT ![d] = @ + 1]
    /\ claims' = claims \cup {o}
    /\ runs' = runs \cup {o}
    /\ claimRevision' = [claimRevision EXCEPT ![o] = revision[d] + 1]
    /\ UNCHANGED <<present, status, scheduled, observed, archived, staleRejected>>

StaleClaim(r, o) ==
    LET d == OccurrenceDeployment(o) IN
    /\ present[d]
    /\ observed[r][d] # revision[d]
    /\ staleRejected' = TRUE
    /\ UNCHANGED <<present, revision, status, scheduled, observed,
                    claims, runs, claimRevision, archived>>

Next ==
    \/ \E r \in Replicas, d \in Deployments : Read(r, d)
    \/ \E d \in Deployments, s \in BOOLEAN : Create(d, s)
    \/ \E r \in Replicas, d \in Deployments,
          st \in Statuses, s \in BOOLEAN : Write(r, d, st, s)
    \/ \E r \in Replicas, d \in Deployments : StaleWrite(r, d)
    \/ \E r \in Replicas, o \in Occurrences : Claim(r, o)
    \/ \E r \in Replicas, o \in Occurrences : StaleClaim(r, o)

TypeOK ==
    /\ present \in [Deployments -> BOOLEAN]
    /\ revision \in [Deployments -> 0..MaxRevision]
    /\ status \in [Deployments -> Statuses]
    /\ scheduled \in [Deployments -> BOOLEAN]
    /\ observed \in [Replicas -> [Deployments -> 0..MaxRevision]]
    /\ claims \subseteq Occurrences
    /\ runs \subseteq Occurrences
    /\ claimRevision \in [Occurrences -> 0..MaxRevision]
    /\ archived \subseteq Deployments
    /\ staleRejected \in BOOLEAN

ScheduledCapacityNeverExceeded ==
    ScheduledCount(present, scheduled, status) <= ScheduledCapacity

ClaimAndRunCommitAtomically == claims = runs

EveryClaimIsRevisionFenced ==
    \A o \in claims :
        /\ claimRevision[o] > 0
        /\ claimRevision[o] <= revision[OccurrenceDeployment(o)]

ArchiveIsAbsorbing == \A d \in archived : status[d] = "archived"

AbsentRowsHaveNoClaims ==
    \A o \in claims : present[OccurrenceDeployment(o)]

Spec == Init /\ [][Next]_vars
=============================================================================
