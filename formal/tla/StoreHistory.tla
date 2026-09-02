----------------------------- MODULE StoreHistory -----------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Threads, Runs, Operations, Payloads, RunStates, TerminalStates,
          NoReceipt, NoOperation, NoPayload

ASSUME /\ Threads /= {}
       /\ Runs /= {}
       /\ Operations /= {}
       /\ Payloads /= {}
       /\ TerminalStates \subseteq RunStates
       /\ NoReceipt \notin Payloads
       /\ NoOperation \notin Operations
       /\ NoPayload \notin Payloads

VARIABLES history, receipts

vars == <<history, receipts>>

HistoryIndexes == 1..Len(history)

ThreadVersion(thread) ==
    Cardinality({index \in HistoryIndexes : history[index].thread = thread})

RunOrdinal(run) ==
    Cardinality({index \in HistoryIndexes : history[index].run = run})

RunIndexes(run) ==
    {index \in HistoryIndexes : history[index].run = run}

LastRunIndex(run) ==
    IF RunIndexes(run) = {}
    THEN 0
    ELSE CHOOSE index \in RunIndexes(run) :
             \A other \in RunIndexes(run) : other <= index

TransitionAllowed(run, nextState) ==
    LET index == LastRunIndex(run)
    IN IF index = 0
       THEN TRUE
       ELSE history[index].state \notin TerminalStates

Init ==
    /\ history = <<>>
    /\ receipts = [operation \in Operations |-> NoReceipt]

AppendRecord(thread, run, state, operation, payload) ==
    Append(history,
        [sequence |-> Len(history) + 1,
         thread |-> thread,
         run |-> run,
         state |-> state,
         operation |-> operation,
         payload |-> payload])

ApplyPlainCommit(thread, run, state) ==
    /\ TransitionAllowed(run, state)
    /\ history' = AppendRecord(
         thread, run, state, NoOperation, NoPayload)
    /\ UNCHANGED receipts

OperationAcceptable(thread, run, state, operation,
                    expectedThreadVersion, expectedRunOrdinal) ==
    /\ receipts[operation] = NoReceipt
    /\ expectedThreadVersion = ThreadVersion(thread)
    /\ expectedRunOrdinal = RunOrdinal(run)
    /\ TransitionAllowed(run, state)

ApplyOperation(thread, run, state, operation, payload,
               expectedThreadVersion, expectedRunOrdinal) ==
    /\ OperationAcceptable(thread, run, state, operation,
                           expectedThreadVersion, expectedRunOrdinal)
    /\ history' = AppendRecord(thread, run, state, operation, payload)
    /\ receipts' = [receipts EXCEPT
         ![operation] =
           [payload |-> payload,
            sequence |-> Len(history'),
            thread |-> thread,
            threadVersion |-> expectedThreadVersion + 1,
            expectedThreadVersion |-> expectedThreadVersion,
            runOrdinal |-> expectedRunOrdinal]]

RetrySomeExactOperation ==
    /\ \E operation \in Operations, payload \in Payloads :
         /\ receipts[operation] /= NoReceipt
         /\ receipts[operation].payload = payload
    /\ UNCHANGED vars

OperationRejected(thread, run, state, operation, payload,
                  expectedThreadVersion, expectedRunOrdinal) ==
    /\ \/ /\ receipts[operation] /= NoReceipt
           /\ receipts[operation].payload /= payload
       \/ /\ receipts[operation] = NoReceipt
           /\ ~OperationAcceptable(thread, run, state, operation,
                                   expectedThreadVersion, expectedRunOrdinal)

RejectSomeOperation ==
    /\ \E thread \in Threads, run \in Runs, state \in RunStates,
          operation \in Operations, payload \in Payloads,
          expectedThreadVersion \in 0..(Len(history) + 1),
          expectedRunOrdinal \in 0..(Len(history) + 1) :
         OperationRejected(thread, run, state, operation, payload,
                           expectedThreadVersion, expectedRunOrdinal)
    /\ UNCHANGED vars

Next ==
    \/ \E thread \in Threads, run \in Runs, state \in RunStates :
         ApplyPlainCommit(thread, run, state)
    \/ \E thread \in Threads, run \in Runs, state \in RunStates,
          operation \in Operations, payload \in Payloads,
          expectedThreadVersion \in 0..(Len(history) + 1),
          expectedRunOrdinal \in 0..(Len(history) + 1) :
         ApplyOperation(thread, run, state, operation, payload,
                        expectedThreadVersion, expectedRunOrdinal)
    \/ RetrySomeExactOperation
    \/ RejectSomeOperation

Spec == Init /\ [][Next]_vars

\* Decision-table coverage needs at most three accepted commits: two expose a
\* same-version CAS race or a terminal successor, and the third distinguishes
\* another Thread/Run/operation without repeating an already-covered relation.
\* The constants still vary every cause axis independently.
StateConstraint == Len(history) <= 3

TypeOK ==
    /\ history \in Seq(
         [sequence : Nat,
          thread : Threads,
          run : Runs,
          state : RunStates,
          operation : Operations \union {NoOperation},
          payload : Payloads \union {NoPayload}])
    /\ receipts \in [Operations ->
         {NoReceipt} \union
         [payload : Payloads,
          sequence : Nat,
          thread : Threads,
          threadVersion : Nat,
          expectedThreadVersion : Nat,
          runOrdinal : Nat]]

HistorySequenceIsDenseAndExact ==
    \A index \in HistoryIndexes : history[index].sequence = index

TerminalRunIsFinal ==
    \A earlier, later \in HistoryIndexes :
      (earlier < later
       /\ history[earlier].run = history[later].run
       /\ history[earlier].state \in TerminalStates)
      => FALSE

ReceiptRefinesOneExactHistoryRecord ==
    \A operation \in Operations :
      receipts[operation] /= NoReceipt =>
        LET receipt == receipts[operation]
            record == history[receipt.sequence]
        IN /\ receipt.sequence \in HistoryIndexes
           /\ record.operation = operation
           /\ record.payload = receipt.payload
           /\ record.thread = receipt.thread
           /\ receipt.threadVersion = receipt.expectedThreadVersion + 1

OneCASWinnerPerThreadVersion ==
    \A left, right \in Operations :
      (left /= right
       /\ receipts[left] /= NoReceipt
       /\ receipts[right] /= NoReceipt
       /\ receipts[left].thread = receipts[right].thread)
      => receipts[left].expectedThreadVersion
         /= receipts[right].expectedThreadVersion

ReceiptsNeverOutrunDurableHistory ==
    \A operation \in Operations :
      receipts[operation] /= NoReceipt =>
        receipts[operation].sequence <= Len(history)

=============================================================================
