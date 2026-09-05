------------------------- MODULE CircuitBreaker -------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Permits, ProbeLimits, MaxGeneration

VARIABLE state, generation, active, outstanding, permitGeneration, permitProbe, maxProbes

vars == <<state, generation, active, outstanding, permitGeneration, permitProbe, maxProbes>>

Init == /\ state = "closed"
        /\ generation = 0
        /\ active = {}
        /\ outstanding = {}
        /\ permitGeneration = [p \in Permits |-> 0]
        /\ permitProbe = [p \in Permits |-> FALSE]
        /\ maxProbes \in ProbeLimits

\* Closed calls have the same epoch fence as probes, but a distinct phase.
\* Failure counting/cooldown are abstracted to immediate threshold/expiry;
\* the production Kani harness owns their integer decision boundaries.
AdmitClosed(p) ==
    /\ p \in Permits \ outstanding
    /\ state = "closed"
    /\ outstanding' = outstanding \cup {p}
    /\ permitGeneration' = [permitGeneration EXCEPT ![p] = generation]
    /\ permitProbe' = [permitProbe EXCEPT ![p] = FALSE]
    /\ UNCHANGED <<state, generation, active, maxProbes>>

StartProbe(p) ==
    /\ p \in Permits \ outstanding
    /\ state = "open"
    /\ generation < MaxGeneration
    /\ maxProbes > 0
    /\ state' = "half_open"
    /\ generation' = generation + 1
    /\ active' = {p}
    /\ outstanding' = outstanding \cup {p}
    /\ permitGeneration' = [permitGeneration EXCEPT ![p] = generation + 1]
    /\ permitProbe' = [permitProbe EXCEPT ![p] = TRUE]
    /\ UNCHANGED maxProbes

AdmitPeer(p) ==
    /\ p \in Permits \ outstanding
    /\ state = "half_open"
    /\ Cardinality(active) < maxProbes
    /\ active' = active \cup {p}
    /\ outstanding' = outstanding \cup {p}
    /\ permitGeneration' = [permitGeneration EXCEPT ![p] = generation]
    /\ permitProbe' = [permitProbe EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<state, generation, maxProbes>>

Current(p) == permitGeneration[p] = generation
              /\ ((permitProbe[p] /\ state = "half_open")
                  \/ (~permitProbe[p] /\ state = "closed"))

Success(p) ==
    /\ p \in outstanding
    /\ IF Current(p)
          THEN /\ state' = "closed"
               /\ active' = {}
          ELSE /\ UNCHANGED <<state, active>>
    /\ outstanding' = outstanding \ {p}
    /\ UNCHANGED <<generation, permitGeneration, permitProbe, maxProbes>>

RetryableFailure(p) ==
    /\ p \in outstanding
    /\ IF Current(p)
          THEN /\ state' = "open"
               /\ active' = {}
          ELSE /\ UNCHANGED <<state, active>>
    /\ outstanding' = outstanding \ {p}
    /\ UNCHANGED <<generation, permitGeneration, permitProbe, maxProbes>>

Abandon(p) ==
    /\ p \in outstanding
    /\ IF Current(p) /\ permitProbe[p]
          THEN /\ state' = "open"
               /\ active' = {}
          ELSE /\ UNCHANGED <<state, active>>
    /\ outstanding' = outstanding \ {p}
    /\ UNCHANGED <<generation, permitGeneration, permitProbe, maxProbes>>

BoundedStutter == /\ (generation = MaxGeneration \/ maxProbes = 0)
                  /\ state = "open"
                  /\ UNCHANGED vars

Next == (\E p \in Permits: AdmitClosed(p))
     \/ (\E p \in Permits: StartProbe(p))
     \/ (\E p \in Permits: AdmitPeer(p))
     \/ (\E p \in Permits: Success(p))
     \/ (\E p \in Permits: RetryableFailure(p))
     \/ (\E p \in Permits: Abandon(p))
     \/ BoundedStutter

TypeOK == /\ state \in {"closed", "open", "half_open"}
          /\ generation \in 0..MaxGeneration
          /\ active \subseteq Permits
          /\ outstanding \subseteq Permits
          /\ permitGeneration \in [Permits -> 0..MaxGeneration]
          /\ permitProbe \in [Permits -> BOOLEAN]
          /\ maxProbes \in ProbeLimits

ProbeCapacity == Cardinality(active) <= maxProbes
ActiveIsOutstanding == active \subseteq outstanding
ActiveOnlyHalfOpen == active # {} => state = "half_open"
ActiveBelongsToCurrentGeneration ==
    \A p \in active: permitGeneration[p] = generation /\ permitProbe[p]
ClosedHasNoProbe == state = "closed" => active = {}

\* S1 late Closed or probe completion at a mismatched phase/epoch may retire
\* its own outstanding token, but cannot change current circuit authority.
StaleCompletionStutters == [][\A p \in Permits:
    (~Current(p) /\ (Success(p) \/ RetryableFailure(p) \/ Abandon(p)))
      => UNCHANGED <<state, generation, active>>]_vars
GenerationNeverWraps == [][generation' >= generation]_vars

Spec == Init /\ [][Next]_vars
=============================================================================
