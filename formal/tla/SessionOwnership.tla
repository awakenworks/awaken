------------------------ MODULE SessionOwnership ------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Sessions, Owners, NoOwner, MaxGeneration

NoRow == [present |-> FALSE, owner |-> NoOwner, generation |-> 0]

VARIABLE rows, reads

vars == <<rows, reads>>

Init == /\ rows = [s \in Sessions |-> NoRow]
        /\ reads = [s \in Sessions |-> NoRow]

SaveOwned(s, o) ==
    /\ s \in Sessions
    /\ o \in Owners
    /\ rows[s].generation < MaxGeneration
    /\ rows' = [rows EXCEPT ![s] =
         [present |-> TRUE, owner |-> o,
          generation |-> @.generation + 1]]
    /\ UNCHANGED reads

Read(s) == /\ s \in Sessions
           /\ reads' = [reads EXCEPT ![s] = rows[s]]
           /\ UNCHANGED rows

Crash == UNCHANGED vars

Next == (\E s \in Sessions, o \in Owners: SaveOwned(s, o))
     \/ (\E s \in Sessions: Read(s))
     \/ Crash

TypeOK ==
    /\ rows \in [Sessions -> [present : BOOLEAN,
                               owner : Owners \cup {NoOwner},
                               generation : 0..MaxGeneration]]
    /\ reads \in [Sessions -> [present : BOOLEAN,
                                owner : Owners \cup {NoOwner},
                                generation : 0..MaxGeneration]]

VisibleRowAlwaysHasOwner ==
    \A s \in Sessions: rows[s].present => rows[s].owner \in Owners

ReadSnapshotIsCoherent ==
    \A s \in Sessions: reads[s].present => reads[s].owner \in Owners

OwnerAndGenerationAreOneFact ==
    \A s \in Sessions: rows[s].present <=> rows[s].generation > 0

Spec == Init /\ [][Next]_vars
=============================================================================
