------------------------- MODULE SessionRealizationMutex -------------------------
EXTENDS Naturals

CONSTANTS DriverA, DriverB, NoDriver
ASSUME /\ DriverA # DriverB /\ NoDriver \notin {DriverA, DriverB}

Drivers == {DriverA, DriverB}
VARIABLES owner, phase, completed
vars == <<owner, phase, completed>>

Init == /\ owner = NoDriver /\ phase = "Idle" /\ completed = 0

Acquire(d) ==
  /\ d \in Drivers
  /\ owner = NoDriver
  /\ owner' = d
  /\ phase' = "Driving"
  /\ UNCHANGED completed

Advance(d) ==
  /\ owner = d
  /\ phase = "Driving"
  /\ phase' = "Committing"
  /\ UNCHANGED <<owner, completed>>

Release(d) ==
  /\ owner = d
  /\ phase = "Committing"
  /\ owner' = NoDriver
  /\ phase' = "Idle"
  /\ completed' = completed + 1

Next == \E d \in Drivers : Acquire(d) \/ Advance(d) \/ Release(d)
Spec == Init /\ [][Next]_vars

TypeOK == /\ owner \in Drivers \cup {NoDriver}
          /\ phase \in {"Idle", "Driving", "Committing"}
          /\ completed \in Nat
OneDriver == phase = "Idle" <=> owner = NoDriver
Safety == TypeOK /\ OneDriver
=============================================================================
