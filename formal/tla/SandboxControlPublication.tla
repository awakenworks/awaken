-------------------- MODULE SandboxControlPublication --------------------
EXTENDS Naturals

CONSTANT MaxLeases

Phases == {"Vacant", "Active", "Closed"}
Leases == 1..MaxLeases

VARIABLES phase, nextLease, activeLease, staleLease, closedOnce

vars == <<phase, nextLease, activeLease, staleLease, closedOnce>>

\* Cause/effect design:
\* - reserving a vacant slot publishes one fresh owner identity;
\* - releasing the current identity vacates the slot, while releasing a stale
\*   identity has no effect on a newer owner;
\* - closing from any live phase is absorbing;
\* - exhausting the finite model stops allocation instead of modelling wrap.
\* The invariants below cover those four decision rules for every interleaving.

Init ==
    /\ phase = "Vacant"
    /\ nextLease = 1
    /\ activeLease = 0
    /\ staleLease = 0
    /\ closedOnce = FALSE

Reserve ==
    /\ phase = "Vacant"
    /\ nextLease \in Leases
    /\ phase' = "Active"
    /\ activeLease' = nextLease
    /\ nextLease' = nextLease + 1
    /\ UNCHANGED <<staleLease, closedOnce>>

ReleaseCurrent ==
    /\ phase = "Active"
    /\ phase' = "Vacant"
    /\ staleLease' = activeLease
    /\ activeLease' = 0
    /\ UNCHANGED <<nextLease, closedOnce>>

ReleaseStale ==
    /\ phase = "Active"
    /\ staleLease \in Leases
    /\ staleLease # activeLease
    /\ UNCHANGED vars

Close ==
    /\ phase # "Closed"
    /\ phase' = "Closed"
    /\ activeLease' = 0
    /\ staleLease' = IF phase = "Active" THEN activeLease ELSE staleLease
    /\ closedOnce' = TRUE
    /\ UNCHANGED nextLease

RepeatClose ==
    /\ phase = "Closed"
    /\ UNCHANGED vars

Next == Reserve \/ ReleaseCurrent \/ ReleaseStale \/ Close \/ RepeatClose

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in Phases
    /\ nextLease \in 1..(MaxLeases + 1)
    /\ activeLease \in 0..MaxLeases
    /\ staleLease \in 0..MaxLeases
    /\ closedOnce \in BOOLEAN

ExactlyOneOwnerWhileActive ==
    (phase = "Active") <=> (activeLease \in Leases)

FreshIdentityNeverReusesStaleLease ==
    /\ (phase = "Active" => activeLease < nextLease)
    /\ (staleLease # 0 => activeLease # staleLease)

CloseIsAbsorbing ==
    closedOnce <=> (phase = "Closed")

ExhaustionCannotWrap ==
    nextLease = MaxLeases + 1 => activeLease < nextLease

=============================================================================
