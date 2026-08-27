----------------------- MODULE ToolResultProtocol -----------------------
EXTENDS Naturals, FiniteSets, TLC

\* Calls are public tool-request occurrences. Operations are immutable delivery
\* attempts retained by the Session Event batch and ResumeApplied audit fact.
CONSTANT Calls, Operations, None, MaxRejected
ASSUME /\ Calls # {} /\ Operations # {} /\ MaxRejected \in Nat

VARIABLE awaiting, completed, receiptCall, accepted, rejected,
         lastAccepted, lastExpected, terminal

vars == <<awaiting, completed, receiptCall, accepted, rejected,
          lastAccepted, lastExpected, terminal>>

Init == /\ awaiting = None
        /\ completed = {}
        /\ receiptCall = [op \in Operations |-> None]
        /\ accepted = [op \in Operations |-> 0]
        /\ rejected = 0
        /\ lastAccepted = None
        /\ lastExpected = None
        /\ terminal = FALSE

ConsumedCalls == {receiptCall[op] : op \in completed}

Await(call) ==
    /\ call \in Calls
    /\ ~terminal
    /\ awaiting = None
    /\ call \notin ConsumedCalls
    /\ awaiting' = call
    /\ UNCHANGED <<completed, receiptCall, accepted, rejected,
                    lastAccepted, lastExpected, terminal>>

\* Ticket consumption and the durable operation receipt are one transition.
Deliver(call, op) ==
    /\ call \in Calls
    /\ op \in Operations
    /\ ~terminal
    /\ awaiting = call
    /\ op \notin completed
    /\ awaiting' = None
    /\ completed' = completed \cup {op}
    /\ receiptCall' = [receiptCall EXCEPT ![op] = call]
    /\ accepted' = [accepted EXCEPT ![op] = @ + 1]
    /\ lastAccepted' = op
    /\ lastExpected' = op
    /\ UNCHANGED <<rejected, terminal>>

\* A response-loss retry observes the exact durable receipt and stutters. It
\* neither needs nor recreates the consumed Awaiting ticket.
Replay(call, op) ==
    /\ call \in Calls
    /\ op \in completed
    /\ receiptCall[op] = call
    /\ UNCHANGED vars

Reject(call, op) ==
    /\ call \in Calls
    /\ op \in Operations
    /\ rejected < MaxRejected
    /\ \/ terminal
       \/ /\ op \in completed
          /\ receiptCall[op] # call
       \/ /\ op \notin completed
          /\ awaiting # call
    /\ rejected' = rejected + 1
    /\ UNCHANGED <<awaiting, completed, receiptCall, accepted,
                    lastAccepted, lastExpected, terminal>>

End == /\ ~terminal
       /\ terminal' = TRUE
       /\ awaiting' = None
       /\ UNCHANGED <<completed, receiptCall, accepted, rejected,
                       lastAccepted, lastExpected>>

Next ==
    (\E call \in Calls, op \in Operations:
        Deliver(call, op) \/ Replay(call, op) \/ Reject(call, op))
    \/ (\E call \in Calls: Await(call))
    \/ End

TypeOK ==
    /\ awaiting \in Calls \cup {None}
    /\ completed \subseteq Operations
    /\ receiptCall \in [Operations -> Calls \cup {None}]
    /\ accepted \in [Operations -> 0..1]
    /\ rejected \in 0..MaxRejected
    /\ lastAccepted \in Operations \cup {None}
    /\ lastExpected \in Operations \cup {None}
    /\ terminal \in BOOLEAN

OnlyMatchingResultIsAccepted ==
    lastAccepted # None => lastAccepted = lastExpected

EveryOperationIsConsumedAtMostOnce ==
    \A op \in Operations: accepted[op] <= 1

ReceiptExistsExactlyForConsumedOperation ==
    \A op \in Operations: (op \in completed) \equiv (receiptCall[op] # None)

OneReceiptNeverNamesTwoCalls ==
    \A op \in completed: receiptCall[op] \in Calls

AwaitingTicketHasNoPriorReceipt ==
    awaiting # None => awaiting \notin ConsumedCalls

TerminalRejectsLateResults == terminal => awaiting = None

Spec == Init /\ [][Next]_vars
=============================================================================
