----------------------- MODULE ToolResultProtocol -----------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Operations, None, MaxRejected
ASSUME /\ Operations # {} /\ MaxRejected \in Nat

VARIABLE awaiting, completed, accepted, rejected, lastAccepted, lastExpected, terminal

vars == <<awaiting, completed, accepted, rejected, lastAccepted, lastExpected, terminal>>

Init == /\ awaiting = None /\ completed = {}
        /\ accepted = [op \in Operations |-> 0]
        /\ rejected = 0 /\ lastAccepted = None /\ lastExpected = None
        /\ terminal = FALSE

Await(op) == /\ ~terminal /\ awaiting = None /\ op \notin completed
             /\ awaiting' = op
             /\ UNCHANGED <<completed, accepted, rejected, lastAccepted, lastExpected, terminal>>

Deliver(op) == /\ ~terminal /\ awaiting = op /\ op \notin completed
               /\ awaiting' = None /\ completed' = completed \cup {op}
               /\ accepted' = [accepted EXCEPT ![op] = @ + 1]
               /\ lastAccepted' = op /\ lastExpected' = op
               /\ UNCHANGED <<rejected, terminal>>

Reject(op) == /\ op \in Operations
              /\ rejected < MaxRejected
              /\ \/ terminal \/ awaiting # op \/ op \in completed
              /\ rejected' = rejected + 1
              /\ UNCHANGED <<awaiting, completed, accepted, lastAccepted, lastExpected, terminal>>

End == /\ ~terminal /\ terminal' = TRUE /\ awaiting' = None
       /\ UNCHANGED <<completed, accepted, rejected, lastAccepted, lastExpected>>

Next == (\E op \in Operations: Await(op) \/ Deliver(op) \/ Reject(op)) \/ End

TypeOK == /\ awaiting \in Operations \cup {None} /\ completed \subseteq Operations
          /\ accepted \in [Operations -> 0..1] /\ rejected \in 0..MaxRejected
          /\ lastAccepted \in Operations \cup {None}
          /\ lastExpected \in Operations \cup {None} /\ terminal \in BOOLEAN
OnlyMatchingResultIsAccepted == lastAccepted # None => lastAccepted = lastExpected
EveryOperationIsConsumedAtMostOnce == \A op \in Operations: accepted[op] <= 1
TerminalRejectsLateResults == terminal => awaiting = None

Spec == Init /\ [][Next]_vars
=============================================================================
