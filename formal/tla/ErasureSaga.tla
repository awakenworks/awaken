--------------------------- MODULE ErasureSaga ---------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Erasers
ASSUME Erasers # {}

VARIABLE completed, removed, stamped, finished, processUp, durableCompleted

vars == <<completed, removed, stamped, finished, processUp, durableCompleted>>

Init == /\ completed = {}
        /\ removed = 0
        /\ stamped = FALSE
        /\ finished = FALSE
        /\ processUp = TRUE
        /\ durableCompleted = {}

Erase(e) == /\ processUp /\ e \in Erasers \ completed
            /\ completed' = completed \cup {e}
            /\ durableCompleted' = completed \cup {e}
            /\ removed' = removed + 1
            /\ UNCHANGED <<stamped, finished, processUp>>

Stamp == /\ processUp /\ completed = Erasers /\ ~stamped
         /\ stamped' = TRUE
         /\ UNCHANGED <<completed, removed, finished, processUp, durableCompleted>>

Finish == /\ processUp /\ stamped /\ ~finished
          /\ finished' = TRUE
          /\ UNCHANGED <<completed, removed, stamped, processUp, durableCompleted>>

Crash == /\ processUp
         /\ processUp' = FALSE
         /\ UNCHANGED <<completed, removed, stamped, finished, durableCompleted>>

Restart == /\ ~processUp
           /\ processUp' = TRUE
           /\ completed' = durableCompleted
           /\ UNCHANGED <<removed, stamped, finished, durableCompleted>>

Next == (\E e \in Erasers: Erase(e)) \/ Stamp \/ Finish \/ Crash \/ Restart

TypeOK == /\ completed \subseteq Erasers /\ durableCompleted \subseteq Erasers
          /\ removed \in Nat /\ stamped \in BOOLEAN /\ finished \in BOOLEAN
          /\ processUp \in BOOLEAN
CheckpointNeverForgets == durableCompleted \subseteq completed
NoEraserIsCountedTwice == removed = Cardinality(durableCompleted)
StampRequiresEveryEraser == stamped => completed = Erasers
CompletionRequiresAccountability == finished => stamped

Spec == Init /\ [][Next]_vars
=============================================================================
