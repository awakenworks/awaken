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

\* Copy-mode refinement boundary: existing CAS/delete actions remain the only
\* durable writer. Each projection captures one base before editing; crashes
\* lose only its live handle, not the persisted materialization evidence.
\* Rules C1 two mounted writers => one CAS winner; C2 update/delete => one
\* linearization; C3 commit then crash before acknowledgement => idempotent
\* same-content retry; C4 intervening writer => stale retry cannot rebase.
\* Tests: copy_lifecycle.rs concurrent_copy_harvest_* / recovered_copy_retries_*.
\* Bounds: one pending edit per writer, atomic Repository calls. A complete
\* quiescent-tree scan is an external observation: the model checks admission
\* on that observation, not OS snapshot atomicity or hostile process fencing.
\* This is safety checking, not a proof of eventual recovery or Rust refinement.
VARIABLES mounted, crashed, materializationBase, scanComplete, lastMutationScan
copyVars == <<vars, mounted, crashed, materializationBase, scanComplete, lastMutationScan>>
CopyInit == /\ Init
            /\ mounted = [w \in Writers |-> FALSE]
            /\ crashed = [w \in Writers |-> FALSE]
            /\ materializationBase = [w \in Writers |-> 0]
            /\ scanComplete = [w \in Writers |-> TRUE]
            /\ lastMutationScan = TRUE

CopyCapture(w) == /\ ~mounted[w] /\ headPresent
                  /\ Read(w)
                  /\ mounted' = [mounted EXCEPT ![w] = TRUE]
                  /\ materializationBase' = [materializationBase EXCEPT ![w] = generation]
                  /\ UNCHANGED <<crashed, scanComplete, lastMutationScan>>

CopyFlush(w) == /\ mounted[w] /\ ~crashed[w]
                /\ scanComplete[w]
                /\ lastMutationScan' = scanComplete[w]
                /\ IF headPresent /\ value = w
                      THEN IdempotentWrite
                      ELSE CAS(w) \/ StaleCAS(w)
                /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete>>

CopyDelete(w) == /\ mounted[w] /\ ~crashed[w]
                 /\ scanComplete[w]
                 /\ lastMutationScan' = scanComplete[w]
                 /\ DeleteIfMatch(w) \/ StaleDelete(w) \/ AlreadyAbsentDelete
                 /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete>>

CopyCrash(w) == /\ mounted[w] /\ ~crashed[w]
                /\ crashed' = [crashed EXCEPT ![w] = TRUE]
                /\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan>>

CopyRecover(w) == /\ crashed[w]
                  /\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan>>

\* A missing root, scan IO error, or replaced entry cannot stand in for unlink.
\* Retrying after the tree is restored changes only scan evidence, never CAS base.
CopyScan(w, complete) ==
    /\ mounted[w] /\ ~crashed[w]
    /\ scanComplete' = [scanComplete EXCEPT ![w] = complete]
    /\ UNCHANGED <<vars, mounted, crashed, materializationBase, lastMutationScan>>

CopyNext == \E w \in Writers:
              CopyCapture(w) \/ CopyFlush(w) \/ CopyDelete(w)
              \/ CopyCrash(w) \/ CopyRecover(w)
              \/ CopyScan(w, TRUE) \/ CopyScan(w, FALSE)
CopyTypeOK == /\ TypeOK
              /\ mounted \in [Writers -> BOOLEAN]
              /\ crashed \in [Writers -> BOOLEAN]
              /\ materializationBase \in [Writers -> 0..MaxGeneration]
              /\ scanComplete \in [Writers -> BOOLEAN]
              /\ lastMutationScan \in BOOLEAN
CopyRecoveryKeepsMaterializationBase ==
    \A w \in Writers: mounted[w] => expected[w] = materializationBase[w]
CopyMutationRequiresCompleteScan == lastAction \in {"cas", "delete"} => lastMutationScan
CopySpec == CopyInit /\ [][CopyNext]_copyVars
\* The original Repository configuration keeps projection history inert.
Spec == CopyInit /\ [][Next /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete, lastMutationScan>>]_copyVars
=============================================================================
