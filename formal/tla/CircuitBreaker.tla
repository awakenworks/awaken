------------------------- MODULE CircuitBreaker -------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Permits, MaxProbes, MaxGeneration

VARIABLE state, generation, active, outstanding, permitGeneration

vars == <<state, generation, active, outstanding, permitGeneration>>

Init == /\ state = "open"
        /\ generation = 0
        /\ active = {}
        /\ outstanding = {}
        /\ permitGeneration = [p \in Permits |-> 0]

StartProbe(p) ==
    /\ p \in Permits \ outstanding
    /\ state = "open"
    /\ generation < MaxGeneration
    /\ state' = "half_open"
    /\ generation' = generation + 1
    /\ active' = {p}
    /\ outstanding' = outstanding \cup {p}
    /\ permitGeneration' = [permitGeneration EXCEPT ![p] = generation + 1]

AdmitPeer(p) ==
    /\ p \in Permits \ outstanding
    /\ state = "half_open"
    /\ Cardinality(active) < MaxProbes
    /\ active' = active \cup {p}
    /\ outstanding' = outstanding \cup {p}
    /\ permitGeneration' = [permitGeneration EXCEPT ![p] = generation]
    /\ UNCHANGED <<state, generation>>

Success(p) ==
    /\ p \in outstanding
    /\ IF permitGeneration[p] = generation /\ state = "half_open"
          THEN /\ state' = "closed"
               /\ active' = {}
          ELSE /\ UNCHANGED <<state, active>>
    /\ outstanding' = outstanding \ {p}
    /\ UNCHANGED <<generation, permitGeneration>>

FailOrAbandon(p) ==
    /\ p \in outstanding
    /\ IF permitGeneration[p] = generation /\ state = "half_open"
          THEN /\ state' = "open"
               /\ active' = {}
          ELSE /\ UNCHANGED <<state, active>>
    /\ outstanding' = outstanding \ {p}
    /\ UNCHANGED <<generation, permitGeneration>>

Reopen == /\ state = "closed"
          /\ state' = "open"
          /\ active' = {}
          /\ UNCHANGED <<generation, outstanding, permitGeneration>>

BoundedStutter == /\ generation = MaxGeneration
                  /\ state = "open"
                  /\ UNCHANGED vars

Next == (\E p \in Permits: StartProbe(p))
     \/ (\E p \in Permits: AdmitPeer(p))
     \/ (\E p \in Permits: Success(p))
     \/ (\E p \in Permits: FailOrAbandon(p))
     \/ Reopen
     \/ BoundedStutter

TypeOK == /\ state \in {"closed", "open", "half_open"}
          /\ generation \in 0..MaxGeneration
          /\ active \subseteq Permits
          /\ outstanding \subseteq Permits
          /\ permitGeneration \in [Permits -> 0..MaxGeneration]

ProbeCapacity == Cardinality(active) <= MaxProbes
ActiveIsOutstanding == active \subseteq outstanding
ActiveOnlyHalfOpen == active # {} => state = "half_open"
ActiveBelongsToCurrentGeneration ==
    \A p \in active: permitGeneration[p] = generation
ClosedHasNoProbe == state = "closed" => active = {}

Spec == Init /\ [][Next]_vars
=============================================================================
