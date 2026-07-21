----------------------------- MODULE MemoryCAS -----------------------------
EXTENDS Naturals, TLC

CONSTANT Writers, NoWriter, MaxGeneration, SourceId, TargetId

VARIABLE generation, value, expected, writes, sourcePresent, targetPresent,
         targetIdentity, renameCommitted, headPresent, lastAction, lastBefore,
         lastAfter, lastPresentBefore, lastPresentAfter

vars == <<generation, value, expected, writes, sourcePresent, targetPresent,
          targetIdentity, renameCommitted, headPresent, lastAction, lastBefore,
          lastAfter, lastPresentBefore, lastPresentAfter>>

Init == /\ generation = 0 /\ value = NoWriter
        /\ expected = [w \in Writers |-> 0]
        /\ writes = [g \in 0..MaxGeneration |-> 0]
        /\ sourcePresent = TRUE /\ targetPresent = TRUE
        /\ targetIdentity = TargetId /\ renameCommitted = FALSE
        /\ headPresent = TRUE
        /\ lastAction = "init" /\ lastBefore = 0 /\ lastAfter = 0
        /\ lastPresentBefore = TRUE /\ lastPresentAfter = TRUE

Read(w) == /\ expected' = [expected EXCEPT ![w] = generation]
           /\ lastAction' = "read" /\ lastBefore' = generation
           /\ lastAfter' = generation
           /\ lastPresentBefore' = headPresent /\ lastPresentAfter' = headPresent
           /\ UNCHANGED <<generation, value, writes, sourcePresent, targetPresent,
                           targetIdentity, renameCommitted, headPresent>>

CAS(w) == /\ headPresent /\ generation < MaxGeneration /\ expected[w] = generation
          /\ generation' = generation + 1 /\ value' = w
          /\ writes' = [writes EXCEPT ![generation + 1] = @ + 1]
          /\ lastAction' = "cas" /\ lastBefore' = generation
          /\ lastAfter' = generation + 1
          /\ lastPresentBefore' = headPresent /\ lastPresentAfter' = headPresent
          /\ UNCHANGED <<expected, sourcePresent, targetPresent,
                          targetIdentity, renameCommitted, headPresent>>

StaleCAS(w) == /\ expected[w] # generation
               /\ lastAction' = "stale_cas" /\ lastBefore' = generation
               /\ lastAfter' = generation
               /\ lastPresentBefore' = headPresent /\ lastPresentAfter' = headPresent
               /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                               targetPresent, targetIdentity, renameCommitted, headPresent>>

IdempotentWrite == /\ value # NoWriter
                   /\ lastAction' = "idempotent_write"
                   /\ lastBefore' = generation /\ lastAfter' = generation
                   /\ lastPresentBefore' = headPresent /\ lastPresentAfter' = headPresent
                   /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                                   targetPresent, targetIdentity, renameCommitted, headPresent>>

DeleteIfMatch(w) == /\ headPresent /\ expected[w] = generation
                    /\ headPresent' = FALSE
                    /\ lastAction' = "delete" /\ lastBefore' = generation
                    /\ lastAfter' = generation /\ lastPresentBefore' = TRUE
                    /\ lastPresentAfter' = FALSE
                    /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                                    targetPresent, targetIdentity, renameCommitted>>

StaleDelete(w) == /\ headPresent /\ expected[w] # generation
                  /\ lastAction' = "stale_delete" /\ lastBefore' = generation
                  /\ lastAfter' = generation /\ lastPresentBefore' = TRUE
                  /\ lastPresentAfter' = TRUE
                  /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                                  targetPresent, targetIdentity, renameCommitted, headPresent>>

AlreadyAbsentDelete == /\ ~headPresent
                       /\ lastAction' = "absent_delete" /\ lastBefore' = generation
                       /\ lastAfter' = generation /\ lastPresentBefore' = FALSE
                       /\ lastPresentAfter' = FALSE
                       /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                                       targetPresent, targetIdentity, renameCommitted, headPresent>>

RenameReplace == /\ sourcePresent /\ ~renameCommitted
                 /\ sourcePresent' = FALSE /\ targetPresent' = TRUE
                 /\ targetIdentity' = SourceId /\ renameCommitted' = TRUE
                 /\ lastAction' = "rename" /\ lastBefore' = generation
                 /\ lastAfter' = generation
                 /\ lastPresentBefore' = headPresent /\ lastPresentAfter' = headPresent
                 /\ UNCHANGED <<generation, value, expected, writes, headPresent>>

Next == (\E w \in Writers: Read(w) \/ CAS(w) \/ StaleCAS(w)
                            \/ DeleteIfMatch(w) \/ StaleDelete(w))
        \/ AlreadyAbsentDelete \/ IdempotentWrite \/ RenameReplace

TypeOK == /\ generation \in 0..MaxGeneration
          /\ value \in Writers \cup {NoWriter}
          /\ expected \in [Writers -> 0..MaxGeneration]
          /\ writes \in [0..MaxGeneration -> 0..1]
          /\ sourcePresent \in BOOLEAN /\ targetPresent \in BOOLEAN
          /\ targetIdentity \in {SourceId, TargetId} /\ renameCommitted \in BOOLEAN
          /\ headPresent \in BOOLEAN
          /\ lastAction \in {"init", "read", "cas", "stale_cas", "idempotent_write",
                              "delete", "stale_delete", "absent_delete", "rename"}
          /\ lastBefore \in 0..MaxGeneration /\ lastAfter \in 0..MaxGeneration
          /\ lastPresentBefore \in BOOLEAN /\ lastPresentAfter \in BOOLEAN
OneWinnerPerGeneration == \A g \in 1..MaxGeneration: writes[g] <= 1
GenerationMatchesWrites == generation = 0 <=> value = NoWriter
StaleCASDoesNotAdvance == lastAction = "stale_cas" => lastBefore = lastAfter
IdempotentWritesDoNotAdvance == lastAction = "idempotent_write" => lastBefore = lastAfter
RenamePreservesSourceIdentity == renameCommitted => (~sourcePresent /\ targetPresent /\ targetIdentity = SourceId)
StaleDeleteDoesNotRemove == lastAction = "stale_delete" => (lastPresentBefore /\ lastPresentAfter)
MatchedDeleteRemovesHead == lastAction = "delete" => (lastPresentBefore /\ ~lastPresentAfter)
AbsentDeleteIsIdempotent == lastAction = "absent_delete" => (~lastPresentBefore /\ ~lastPresentAfter)

Spec == Init /\ [][Next]_vars
=============================================================================
