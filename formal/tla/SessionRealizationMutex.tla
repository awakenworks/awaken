------------------------- MODULE SessionRealizationMutex -------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS DriverA, DriverB, NoDriver, Sessions, MaxCompleted, MaxConcurrent
ASSUME /\ DriverA # DriverB
       /\ NoDriver \notin {DriverA, DriverB}
       /\ Sessions # {}
       /\ IsFiniteSet(Sessions)
       /\ MaxCompleted \in Nat \ {0}
       /\ MaxConcurrent \in Nat \ {0}
       /\ MaxConcurrent <= Cardinality(Sessions)

Drivers == {DriverA, DriverB}
VARIABLES owner, phase, completed
vars == <<owner, phase, completed>>

ActiveSessions == {session \in Sessions : owner[session] # NoDriver}

Init == /\ owner = [session \in Sessions |-> NoDriver]
        /\ phase = [session \in Sessions |-> "Idle"]
        /\ completed = [session \in Sessions |-> 0]

Acquire(session, d) ==
  /\ session \in Sessions
  /\ d \in Drivers
  /\ owner[session] = NoDriver
  /\ Cardinality(ActiveSessions) < MaxConcurrent
  /\ owner' = [owner EXCEPT ![session] = d]
  /\ phase' = [phase EXCEPT ![session] = "Driving"]
  /\ UNCHANGED completed

Advance(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Driving"
  /\ phase' = [phase EXCEPT ![session] = "Committing"]
  /\ UNCHANGED <<owner, completed>>

Release(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Committing"
  /\ completed[session] < MaxCompleted
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ completed' = [completed EXCEPT ![session] = @ + 1]

Next == \E session \in Sessions, d \in Drivers :
          Acquire(session, d) \/ Advance(session, d) \/ Release(session, d)
Spec == Init /\ [][Next]_vars

TypeOK == /\ owner \in [Sessions -> Drivers \cup {NoDriver}]
          /\ phase \in [Sessions -> {"Idle", "Driving", "Committing"}]
          /\ completed \in [Sessions -> 0..MaxCompleted]
OneDriverPerSession ==
  \A session \in Sessions :
    phase[session] = "Idle" <=> owner[session] = NoDriver
BoundedCrossSessionConcurrency ==
  Cardinality(ActiveSessions) <= MaxConcurrent
Safety == TypeOK /\ OneDriverPerSession /\ BoundedCrossSessionConcurrency
=============================================================================
