---------------------------- MODULE ConfigCAS ----------------------------
EXTENDS Naturals, TLC

CONSTANT Writers, MaxGeneration, NoWriter

VARIABLE generation, value, expected, result, resultExpected, resultGeneration,
         lastApplied, lastBefore, lastAfter

vars == <<generation, value, expected, result, resultExpected, resultGeneration,
          lastApplied, lastBefore, lastAfter>>

Init == /\ generation = 0
        /\ value = NoWriter
        /\ expected = [w \in Writers |-> 0]
        /\ result = [w \in Writers |-> "none"]
        /\ resultExpected = [w \in Writers |-> 0]
        /\ resultGeneration = [w \in Writers |-> 0]
        /\ lastApplied = FALSE
        /\ lastBefore = 0
        /\ lastAfter = 0

Read(w) == /\ w \in Writers
           /\ expected' = [expected EXCEPT ![w] = generation]
           /\ UNCHANGED <<generation, value, result,
                           resultExpected, resultGeneration, lastApplied,
                           lastBefore, lastAfter>>

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
    /\ UNCHANGED expected

Next == (\E w \in Writers: Read(w)) \/ (\E w \in Writers: Write(w))

TypeOK == /\ generation \in 0..MaxGeneration
          /\ value \in Writers \cup {NoWriter}
          /\ expected \in [Writers -> 0..MaxGeneration]
          /\ result \in [Writers -> {"none", "applied", "conflict"}]
          /\ resultExpected \in [Writers -> 0..MaxGeneration]
          /\ resultGeneration \in [Writers -> 0..MaxGeneration]
          /\ lastApplied \in BOOLEAN
          /\ lastBefore \in 0..MaxGeneration
          /\ lastAfter \in 0..MaxGeneration

GenerationZeroHasNoValue == (generation = 0) <=> (value = NoWriter)
AppliedMatchedGeneration ==
    \A w \in Writers:
        result[w] = "applied" => resultGeneration[w] = resultExpected[w] + 1
LastWriteIsAtomicCAS ==
    IF lastApplied THEN lastAfter = lastBefore + 1 ELSE lastAfter = lastBefore

Spec == Init /\ [][Next]_vars
=============================================================================
