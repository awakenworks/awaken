----------------------------- MODULE MemoryCAS -----------------------------
EXTENDS Naturals, TLC

CONSTANT Writers, NoWriter, MaxGeneration, SourceId, TargetId

VARIABLE generation, value, expected, writes, sourcePresent, targetPresent,
         targetIdentity, renameCommitted, lastAction, lastBefore, lastAfter

vars == <<generation, value, expected, writes, sourcePresent, targetPresent,
          targetIdentity, renameCommitted, lastAction, lastBefore, lastAfter>>

Init == /\ generation = 0 /\ value = NoWriter
        /\ expected = [w \in Writers |-> 0]
        /\ writes = [g \in 0..MaxGeneration |-> 0]
        /\ sourcePresent = TRUE /\ targetPresent = TRUE
        /\ targetIdentity = TargetId /\ renameCommitted = FALSE
        /\ lastAction = "init" /\ lastBefore = 0 /\ lastAfter = 0

Read(w) == /\ expected' = [expected EXCEPT ![w] = generation]
           /\ lastAction' = "read" /\ lastBefore' = generation
           /\ lastAfter' = generation
           /\ UNCHANGED <<generation, value, writes, sourcePresent, targetPresent,
                           targetIdentity, renameCommitted>>

CAS(w) == /\ generation < MaxGeneration /\ expected[w] = generation
          /\ generation' = generation + 1 /\ value' = w
          /\ writes' = [writes EXCEPT ![generation + 1] = @ + 1]
          /\ lastAction' = "cas" /\ lastBefore' = generation
          /\ lastAfter' = generation + 1
          /\ UNCHANGED <<expected, sourcePresent, targetPresent,
                          targetIdentity, renameCommitted>>

StaleCAS(w) == /\ expected[w] # generation
               /\ lastAction' = "stale_cas" /\ lastBefore' = generation
               /\ lastAfter' = generation
               /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                               targetPresent, targetIdentity, renameCommitted>>

IdempotentWrite == /\ value # NoWriter
                   /\ lastAction' = "idempotent_write"
                   /\ lastBefore' = generation /\ lastAfter' = generation
                   /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                                   targetPresent, targetIdentity, renameCommitted>>

RenameReplace == /\ sourcePresent /\ ~renameCommitted
                 /\ sourcePresent' = FALSE /\ targetPresent' = TRUE
                 /\ targetIdentity' = SourceId /\ renameCommitted' = TRUE
                 /\ lastAction' = "rename" /\ lastBefore' = generation
                 /\ lastAfter' = generation
                 /\ UNCHANGED <<generation, value, expected, writes>>

Next == (\E w \in Writers: Read(w) \/ CAS(w) \/ StaleCAS(w))
        \/ IdempotentWrite \/ RenameReplace

TypeOK == /\ generation \in 0..MaxGeneration
          /\ value \in Writers \cup {NoWriter}
          /\ expected \in [Writers -> 0..MaxGeneration]
          /\ writes \in [0..MaxGeneration -> 0..1]
          /\ sourcePresent \in BOOLEAN /\ targetPresent \in BOOLEAN
          /\ targetIdentity \in {SourceId, TargetId} /\ renameCommitted \in BOOLEAN
          /\ lastAction \in {"init", "read", "cas", "stale_cas", "idempotent_write", "rename"}
          /\ lastBefore \in 0..MaxGeneration /\ lastAfter \in 0..MaxGeneration
OneWinnerPerGeneration == \A g \in 1..MaxGeneration: writes[g] <= 1
GenerationMatchesWrites == generation = 0 <=> value = NoWriter
StaleCASDoesNotAdvance == lastAction = "stale_cas" => lastBefore = lastAfter
IdempotentWritesDoNotAdvance == lastAction = "idempotent_write" => lastBefore = lastAfter
RenamePreservesSourceIdentity == renameCommitted => (~sourcePresent /\ targetPresent /\ targetIdentity = SourceId)

Spec == Init /\ [][Next]_vars
=============================================================================
