----------------------------- MODULE StoreHistory -----------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Threads, Runs, Operations, Payloads, RunStates, TerminalStates,
          NoReceipt, NoOperation, NoPayload, ObservationTimes, UnknownTime

ASSUME /\ Threads /= {}
       /\ Runs /= {}
       /\ Operations /= {}
       /\ Payloads /= {}
       /\ TerminalStates \subseteq RunStates
       /\ NoReceipt \notin Payloads
       /\ NoOperation \notin Operations
       /\ NoPayload \notin Payloads

ASSUME /\ ObservationTimes \subseteq Nat
       /\ ObservationTimes /= {}
       /\ UnknownTime \notin ObservationTimes

VARIABLES history, receipts, recoveredPrefix

vars == <<history, receipts, recoveredPrefix>>

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
    /\ recoveredPrefix = <<>>
    /\ receipts = [operation \in Operations |-> NoReceipt]

AppendRecord(thread, run, state, operation, payload, observedAt) ==
    Append(history,
        [sequence |-> Len(history) + 1,
         thread |-> thread,
         run |-> run,
         state |-> state,
         operation |-> operation,
         payload |-> payload,
         observedAt |-> observedAt])

ApplyPlainCommit(thread, run, state) ==
    /\ Len(history) < 3
    /\ TransitionAllowed(run, state)
    /\ \E observedAt \in ObservationTimes : history' = AppendRecord(
         thread, run, state, NoOperation, NoPayload, observedAt)
    /\ UNCHANGED <<receipts, recoveredPrefix>>

OperationAcceptable(thread, run, state, operation,
                    expectedThreadVersion, expectedRunOrdinal) ==
    /\ receipts[operation] = NoReceipt
    /\ expectedThreadVersion = ThreadVersion(thread)
    /\ expectedRunOrdinal = RunOrdinal(run)
    /\ TransitionAllowed(run, state)

OperationReceipt(thread, payload, sequence, version, ordinal, observedAt) ==
    [payload |-> payload,
     sequence |-> sequence,
     thread |-> thread,
     threadVersion |-> version + 1,
     expectedThreadVersion |-> version,
     runOrdinal |-> ordinal,
     observedAt |-> observedAt]

ApplyOperation(thread, run, state, operation, payload,
               expectedThreadVersion, expectedRunOrdinal) ==
    /\ Len(history) < 3
    /\ OperationAcceptable(thread, run, state, operation,
                           expectedThreadVersion, expectedRunOrdinal)
    /\ \E observedAt \in ObservationTimes :
         history' = AppendRecord(thread, run, state, operation, payload, observedAt)
    /\ UNCHANGED recoveredPrefix
    /\ receipts' = [receipts EXCEPT
         ![operation] =
           OperationReceipt(thread, payload, Len(history'), expectedThreadVersion,
                            expectedRunOrdinal, history'[Len(history')].observedAt)]

\* Exact retries and rejected requests are atomic stutters. Spec's [Next]_vars
\* already includes precisely that relation; do not re-enumerate identical
\* transitions for every rejected payload/version tuple. This preserves the
\* temporal state graph and bounds, rather than introducing another retry path.

\* Legacy fixture insertion represents an old record, not a new writer.
\* Recovery copies only committed prefix records; a process crash discards
\* that cache. Ambiguous commit acknowledgements retry the same receipt above.
LoadLegacy ==
    /\ history = <<>>
    /\ \E thread \in Threads, run \in Runs, state \in RunStates :
         history' = AppendRecord(thread, run, state, NoOperation, NoPayload, UnknownTime)
    /\ UNCHANGED <<receipts, recoveredPrefix>>

Recover ==
    /\ recoveredPrefix' = history
    /\ UNCHANGED <<history, receipts>>

Crash ==
    /\ recoveredPrefix' = <<>>
    /\ UNCHANGED <<history, receipts>>

Next ==
    \/ \E thread \in Threads, run \in Runs, state \in RunStates :
         ApplyPlainCommit(thread, run, state)
    \/ \E thread \in Threads, run \in Runs, state \in RunStates,
          operation \in Operations, payload \in Payloads,
          expectedThreadVersion \in 0..(Len(history) + 1),
          expectedRunOrdinal \in 0..(Len(history) + 1) :
         ApplyOperation(thread, run, state, operation, payload,
                        expectedThreadVersion, expectedRunOrdinal)
    \/ LoadLegacy
    \/ Recover
    \/ Crash

Spec == Init /\ [][Next]_vars

\* Decision-table coverage needs at most three accepted commits: two expose a
\* same-version CAS race or a terminal successor, and the third distinguishes
\* another Thread/Run/operation without repeating an already-covered relation.
\* The constants still vary every cause axis independently.
\* Guard appends at the same bound, avoiding transient out-of-bound records.
StateConstraint == Len(history) <= 3

TypeOK ==
    /\ history \in Seq(
         [sequence : Nat,
          thread : Threads,
          run : Runs,
          state : RunStates,
          operation : Operations \union {NoOperation},
          payload : Payloads \union {NoPayload},
          observedAt : ObservationTimes \union {UnknownTime}])
    /\ receipts \in [Operations ->
         {NoReceipt} \union
         [payload : Payloads,
          sequence : Nat,
          thread : Threads,
          threadVersion : Nat,
          expectedThreadVersion : Nat,
          runOrdinal : Nat,
          observedAt : ObservationTimes]]

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
           /\ record.observedAt = receipt.observedAt
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

CommittedMetadataPrefix(prefix, committed) ==
    /\ Len(prefix) <= Len(committed)
    /\ \A index \in 1..Len(prefix) : prefix[index] = committed[index]

RecoveryPreservesExactCommittedMetadata == CommittedMetadataPrefix(recoveredPrefix, history)

CommittedPrefixIsImmutable ==
    [][\A index \in HistoryIndexes : history'[index] = history[index]]_vars


\* Adapter trace projection. Recovery exposes the next ordinal (zero-based)
\* and the committed cursor. Payload bytes and full-prefix equality are checked
\* by the producer; this law independently checks the recorded protocol coordinates.
LostAcknowledgementRefinement(trace) ==
    IF Len(trace.observations) /= 7 THEN FALSE ELSE
    LET points == trace.observations
        input == trace.input
        before == points[1].state
        after == points[2].state
        oldEpoch == points[1].epoch
        newEpoch == points[4].epoch
    IN /\ [i \in 1..7 |-> points[i].kind] =
               <<"initial", "unknown", "replayed", "takeover", "fenced", "identity-conflict", "replayed">>
       /\ input.conflict_hash # input.hash
       /\ before.version = input.version
       /\ before.ordinal = input.operation.ordinal
       /\ after.version = before.version + 1
       /\ after.ordinal = before.ordinal + 1
       /\ after.messages = before.messages + input.messages
       /\ after.cursor > before.cursor
       /\ \A i \in 2..7 : points[i].state = after
       /\ oldEpoch > 0 /\ newEpoch > oldEpoch
       /\ [i \in 1..7 |-> points[i].epoch] =
               <<oldEpoch, oldEpoch, oldEpoch, newEpoch, oldEpoch, newEpoch, newEpoch>>
       /\ \A i \in {1, 2, 4, 5, 6} : points[i].receipts = <<>>
       /\ Len(points[3].receipts) = 1
       /\ LET receipt == points[3].receipts[1]
          IN /\ receipt.operation_id = input.operation
             /\ receipt.payload_hash = input.hash
             /\ receipt.thread_version = after.version
             /\ receipt.commit_sequence = after.cursor
             /\ receipt.duplicate
             /\ points[7].receipts = <<receipt>>

=============================================================================
