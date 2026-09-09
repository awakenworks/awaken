--------------------------- MODULE AggregateCAS --------------------------
EXTENDS Naturals, TLC

CONSTANT Writers, MaxGeneration, NoWriter

VARIABLE generation, value, expected, result, resultExpected, resultGeneration,
         lastApplied, lastBefore, lastAfter, lastValueBefore, lastValueAfter

vars == <<generation, value, expected, result, resultExpected, resultGeneration,
          lastApplied, lastBefore, lastAfter, lastValueBefore, lastValueAfter>>

Init == /\ generation = 0
        /\ value = NoWriter
        /\ expected = [w \in Writers |-> 0]
        /\ result = [w \in Writers |-> "none"]
        /\ resultExpected = [w \in Writers |-> 0]
        /\ resultGeneration = [w \in Writers |-> 0]
        /\ lastApplied = FALSE
        /\ lastBefore = 0
        /\ lastAfter = 0
        /\ lastValueBefore = NoWriter
        /\ lastValueAfter = NoWriter

Read(w) == /\ w \in Writers
           /\ expected' = [expected EXCEPT ![w] = generation]
           /\ UNCHANGED <<generation, value, result,
                           resultExpected, resultGeneration, lastApplied,
                           lastBefore, lastAfter, lastValueBefore, lastValueAfter>>

\* Client revisions need not originate in this row's history. In particular,
\* a positive revision can target an absent row (wrong scope or stale client).
\* R1 missing/zero -> create; R2 missing/positive -> reject; R3 exact/present
\* -> successor; R4 stale -> reject; R5 exhausted -> unchanged. SQL/storage
\* conformance owns the concrete realization, not this finite abstract model.
Propose(w, revision) ==
    /\ w \in Writers
    /\ revision \in 0..MaxGeneration
    /\ expected' = [expected EXCEPT ![w] = revision]
    /\ UNCHANGED <<generation, value, result, resultExpected, resultGeneration,
                    lastApplied, lastBefore, lastAfter, lastValueBefore, lastValueAfter>>

Write(w) ==
    /\ w \in Writers
    /\ IF expected[w] = generation /\ generation < MaxGeneration
          THEN /\ generation' = generation + 1
               /\ value' = w
               /\ result' = [result EXCEPT ![w] = "applied"]
               /\ resultExpected' = [resultExpected EXCEPT ![w] = expected[w]]
               /\ resultGeneration' = [resultGeneration EXCEPT ![w] = generation + 1]
               /\ lastApplied' = TRUE
               /\ lastBefore' = generation
               /\ lastAfter' = generation + 1
          ELSE /\ UNCHANGED <<generation, value>>
               /\ result' = [result EXCEPT ![w] = "conflict"]
               /\ resultExpected' = [resultExpected EXCEPT ![w] = expected[w]]
               /\ resultGeneration' = [resultGeneration EXCEPT ![w] = generation]
               /\ lastApplied' = FALSE
               /\ lastBefore' = generation
               /\ lastAfter' = generation
    \* Witnesses observe the actual pre/post value, including rejected writes.
    /\ lastValueBefore' = value
    /\ lastValueAfter' = value'
    /\ UNCHANGED expected

Next == (\E w \in Writers: Read(w)) \/ (\E w \in Writers: Write(w))
        \/ (\E w \in Writers, r \in 0..MaxGeneration: Propose(w, r))

TypeOK == /\ generation \in 0..MaxGeneration
          /\ value \in Writers \cup {NoWriter}
          /\ expected \in [Writers -> 0..MaxGeneration]
          /\ result \in [Writers -> {"none", "applied", "conflict"}]
          /\ resultExpected \in [Writers -> 0..MaxGeneration]
          /\ resultGeneration \in [Writers -> 0..MaxGeneration]
          /\ lastApplied \in BOOLEAN
          /\ lastBefore \in 0..MaxGeneration
          /\ lastAfter \in 0..MaxGeneration
          /\ lastValueBefore \in Writers \cup {NoWriter}
          /\ lastValueAfter \in Writers \cup {NoWriter}

GenerationZeroHasNoValue == (generation = 0) <=> (value = NoWriter)
AppliedMatchedGeneration ==
    \A w \in Writers:
        result[w] = "applied" => resultGeneration[w] = resultExpected[w] + 1
LastWriteIsAtomicCAS ==
    /\ generation = lastAfter
    /\ value = lastValueAfter
    /\ IF lastApplied THEN lastAfter = lastBefore + 1 ELSE lastAfter = lastBefore

\* R4/R5 rejection must preserve both fields. A mutant may leave generation
\* unchanged while overwriting a present row's value; numeric CAS invariants
\* alone do not detect that lost update.
RejectedWritePreservesValue == ~lastApplied => lastValueAfter = lastValueBefore

Spec == Init /\ [][Next]_vars
=============================================================================
