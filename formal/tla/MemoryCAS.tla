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
\* Rules L1-L6: admitted request -> caller crash -> late SQL commit -> retry.
\* Test: writeback_faults.rs sqlite_commit_continues_after_caller_abort_and_retry_converges.
\* Bounds: one in-flight edit per writer, atomic Repository commit. A complete
\* quiescent-tree scan is an external observation: the model checks admission
\* on that observation, not OS snapshot atomicity or hostile process fencing.
\* This is safety checking, not a proof of eventual recovery or Rust refinement.
VARIABLES mounted, crashed, materializationBase, scanComplete, lastMutationScan,
          rootMatches, lastMutationRoot, pendingAck, inFlight, requestScan, requestRoot
requestVars == <<inFlight, requestScan, requestRoot>>
copyVars == <<vars, mounted, crashed, materializationBase, scanComplete, lastMutationScan,
              rootMatches, lastMutationRoot, pendingAck, requestVars>>
CopyInit == /\ Init
            /\ mounted = [w \in Writers |-> FALSE]
            /\ crashed = [w \in Writers |-> FALSE]
            /\ materializationBase = [w \in Writers |-> 0]
            /\ scanComplete = [w \in Writers |-> TRUE]
            /\ lastMutationScan = TRUE
            /\ rootMatches = [w \in Writers |-> TRUE]
            /\ lastMutationRoot = TRUE
            /\ pendingAck = [w \in Writers |-> FALSE]
            /\ inFlight = [w \in Writers |-> "none"]
            /\ requestScan = [w \in Writers |-> TRUE]
            /\ requestRoot = [w \in Writers |-> TRUE]

CopyCapture(w) == /\ ~mounted[w] /\ headPresent /\ sourcePresent
                  /\ Read(w)
                  /\ mounted' = [mounted EXCEPT ![w] = TRUE]
                  /\ materializationBase' = [materializationBase EXCEPT ![w] = generation]
                  /\ UNCHANGED <<crashed, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>

\* Admission captures the scan, just as the production walker captures bytes.
\* Later root changes cannot alter that request. Cancellation of the caller
\* does not cancel SQLite spawn_blocking; commit remains the same CAS owner.
CopyStart(w, operation) ==
    /\ mounted[w] /\ ~crashed[w] /\ inFlight[w] = "none"
    /\ scanComplete[w]
    /\ rootMatches[w]
    /\ inFlight' = [inFlight EXCEPT ![w] = operation]
    /\ requestScan' = [requestScan EXCEPT ![w] = scanComplete[w]]
    /\ requestRoot' = [requestRoot EXCEPT ![w] = rootMatches[w]]
    /\ UNCHANGED <<vars, mounted, crashed, materializationBase, scanComplete,
                    lastMutationScan, rootMatches, lastMutationRoot, pendingAck>>

\* MP: a materialized path is a location precondition, unlike an open FUSE fd.
\* Rename can preserve id/hash (and even candidate bytes); check location at
\* commit, BEFORE idempotence. CopyCapture freezes the original source path.
CopyLocationConflict ==
    /\ ~sourcePresent
    /\ lastAction' = "stale_cas"
    /\ lastBefore' = generation /\ lastAfter' = generation
    /\ lastPresentBefore' = headPresent /\ lastPresentAfter' = headPresent
    /\ UNCHANGED <<generation, value, expected, writes, sourcePresent,
                    targetPresent, targetIdentity, renameCommitted, headPresent>>

CopyFlush(w) == /\ inFlight[w] = "write"
                /\ inFlight' = [inFlight EXCEPT ![w] = "none"]
                /\ lastMutationRoot' = requestRoot[w]
                /\ pendingAck' = [pendingAck EXCEPT ![w] = ~crashed[w]]
                /\ lastMutationScan' = requestScan[w]
                /\ IF ~sourcePresent THEN CopyLocationConflict
                   ELSE IF headPresent /\ value = w
                      THEN IdempotentWrite
                      ELSE CAS(w) \/ StaleCAS(w)
                /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete, rootMatches, requestScan, requestRoot>>

CopyDelete(w) == /\ inFlight[w] = "delete"
                 /\ inFlight' = [inFlight EXCEPT ![w] = "none"]
                 /\ lastMutationRoot' = requestRoot[w]
                 /\ pendingAck' = [pendingAck EXCEPT ![w] = ~crashed[w]]
                 /\ lastMutationScan' = requestScan[w]
                 /\ DeleteIfMatch(w) \/ StaleDelete(w) \/ AlreadyAbsentDelete
                 /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete, rootMatches, requestScan, requestRoot>>

CopyCrash(w) == /\ mounted[w] /\ ~crashed[w]
                /\ crashed' = [crashed EXCEPT ![w] = TRUE]
                /\ pendingAck' = [pendingAck EXCEPT ![w] = FALSE]
                /\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, requestVars>>

CopyRecover(w) == /\ crashed[w]
                  /\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>

\* A missing root, scan IO error, or replaced entry cannot stand in for unlink.
\* Retrying after the tree is restored changes only scan evidence, never CAS base.
CopyScan(w, complete) ==
    /\ mounted[w] /\ ~crashed[w]
    /\ scanComplete' = [scanComplete EXCEPT ![w] = complete]
    /\ UNCHANGED <<vars, mounted, crashed, materializationBase, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>

\* Physical root observations and acknowledgement are independent of durable
\* Repository truth. In particular, commit -> lost acknowledgement -> crash
\* cannot undo a committed version. Root restoration cannot refresh a CAS base.
CopyRoot(w, valid) ==
    /\ mounted[w] /\ ~crashed[w]
    /\ rootMatches' = [rootMatches EXCEPT ![w] = valid]
    /\ scanComplete' = [scanComplete EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<vars, mounted, crashed, materializationBase, lastMutationScan, lastMutationRoot, pendingAck, requestVars>>

CopyAcknowledge(w) ==
    /\ pendingAck[w] /\ ~crashed[w]
    /\ pendingAck' = [pendingAck EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<vars, mounted, crashed, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, requestVars>>

CopyRename == /\ \E w \in Writers: mounted[w]
    /\ RenameReplace
    /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete,
                    lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>

CopyNext == CopyRename \/ (\E w \in Writers:
              CopyCapture(w) \/ CopyFlush(w) \/ CopyDelete(w)
              \/ CopyStart(w, "write") \/ CopyStart(w, "delete")
              \/ CopyCrash(w) \/ CopyRecover(w)
              \/ CopyScan(w, TRUE) \/ CopyScan(w, FALSE)
              \/ CopyRoot(w, TRUE) \/ CopyRoot(w, FALSE) \/ CopyAcknowledge(w))
CopyTypeOK == /\ TypeOK
              /\ mounted \in [Writers -> BOOLEAN]
              /\ crashed \in [Writers -> BOOLEAN]
              /\ materializationBase \in [Writers -> 0..MaxGeneration]
              /\ scanComplete \in [Writers -> BOOLEAN]
              /\ lastMutationScan \in BOOLEAN
              /\ rootMatches \in [Writers -> BOOLEAN]
              /\ lastMutationRoot \in BOOLEAN
              /\ pendingAck \in [Writers -> BOOLEAN]
              /\ inFlight \in [Writers -> {"none", "write", "delete"}]
              /\ requestScan \in [Writers -> BOOLEAN]
              /\ requestRoot \in [Writers -> BOOLEAN]
CopyRecoveryKeepsMaterializationBase ==
    \A w \in Writers: mounted[w] => expected[w] = materializationBase[w]
CopyMutationRequiresCompleteScan == lastAction \in {"cas", "delete"} => lastMutationScan
CopyMutationRequiresOriginalRoot == lastAction \in {"cas", "delete"} => lastMutationRoot
CopyCrashKeepsInFlight ==
    [][\A w \in Writers: (~crashed[w] /\ crashed'[w]) => inFlight' = inFlight]_copyVars
CopyUpdateRequiresOriginalLocation ==
    [][(~sourcePresent /\ lastAction' \in {"cas", "idempotent_write"}) => FALSE]_copyVars
CopySpec == CopyInit /\ [][CopyNext]_copyVars
\* The original Repository configuration keeps projection history inert.
Spec == CopyInit /\ [][Next /\ UNCHANGED <<mounted, crashed, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>]_copyVars
=============================================================================
