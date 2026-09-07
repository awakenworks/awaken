----------------------- MODULE CheckpointRecovery -----------------------
EXTENDS Naturals, TLC

CONSTANT MaxChunks

VARIABLE durable, staged, checkpoint, checkpointValid, phase,
         lastKind, lastBefore, lastAfter, readStatus, savedCheckpoint, savedValid

vars == <<durable, staged, checkpoint, checkpointValid, phase,
          lastKind, lastBefore, lastAfter, readStatus, savedCheckpoint, savedValid>>

AcceptedReads == {"absent", "exact"}
RejectedReads == {"storage", "fenced", "wrong-run", "wrong-thread", "cancelled"}

NatMax(a, b) == IF a >= b THEN a ELSE b

Init == /\ durable = 0
        /\ staged = 0
        /\ checkpoint = 0
        /\ checkpointValid = FALSE
        /\ phase = "running"
        /\ lastKind = "init"
        /\ lastBefore = 0
        /\ lastAfter = 0
        /\ readStatus = "absent"
        /\ savedCheckpoint = 0
        /\ savedValid = FALSE

Produce == /\ phase = "running"
           /\ staged < MaxChunks
           /\ staged' = staged + 1
           /\ lastKind' = "produce"
           /\ lastBefore' = durable
           /\ lastAfter' = durable
           /\ UNCHANGED <<durable, checkpoint, checkpointValid, phase,
                          readStatus, savedCheckpoint, savedValid>>

CommitWatermark == /\ phase = "running"
                   /\ durable' = staged
                   /\ lastKind' = "commit"
                   /\ lastBefore' = durable
                   /\ lastAfter' = staged
                   /\ UNCHANGED <<staged, checkpoint, checkpointValid, phase,
                                  readStatus, savedCheckpoint, savedValid>>

FlushCheckpoint == /\ phase = "running"
                   /\ checkpoint' = staged
                   /\ checkpointValid' = TRUE
                   /\ lastKind' = "flush"
                   /\ lastBefore' = durable
                   /\ lastAfter' = durable
                   /\ UNCHANGED <<durable, staged, phase,
                                  readStatus, savedCheckpoint, savedValid>>

Crash == /\ phase = "running"
         /\ phase' = "crashed"
         /\ staged' = durable
         /\ lastKind' = "crash"
         /\ lastBefore' = durable
         /\ lastAfter' = durable
         /\ readStatus' = "pending"
         /\ savedCheckpoint' = checkpoint
         /\ savedValid' = checkpointValid
         /\ UNCHANGED <<durable, checkpoint, checkpointValid>>

\* Decision table: observed absence/exact identity permits existing recovery;
\* storage/fence/foreign identity/cancelled observation never permits effects.
\* A pending read is not absence. Provider termination is an independent gate
\* checked by ModelOperation's production/Kani tests, not inferred by this model.
ObserveRead(outcome) ==
  /\ phase = "crashed" /\ readStatus = "pending"
  /\ outcome \in RejectedReads \cup {IF checkpointValid THEN "exact" ELSE "absent"}
  /\ readStatus' = outcome
  /\ UNCHANGED <<durable, staged, checkpoint, checkpointValid, phase,
                 lastKind, lastBefore, lastAfter, savedCheckpoint, savedValid>>

Recover == /\ phase = "crashed"
           /\ readStatus \in AcceptedReads
           /\ phase' = "running"
           /\ staged' = IF checkpointValid
                            THEN NatMax(durable, checkpoint)
                            ELSE durable
           /\ lastKind' = "recover"
           /\ lastBefore' = durable
           /\ lastAfter' = durable
           /\ UNCHANGED <<durable, checkpoint, checkpointValid,
                          readStatus, savedCheckpoint, savedValid>>

RejectRead == /\ phase = "crashed"
              /\ readStatus \in RejectedReads
              /\ phase' = "blocked"
              /\ lastKind' = "reject"
              /\ lastBefore' = durable
              /\ lastAfter' = durable
              /\ UNCHANGED <<durable, staged, checkpoint, checkpointValid,
                             readStatus, savedCheckpoint, savedValid>>

Complete == /\ phase = "running"
            /\ phase' = "done"
            /\ durable' = staged
            /\ checkpointValid' = FALSE
            /\ lastKind' = "complete"
            /\ lastBefore' = durable
            /\ lastAfter' = staged
            /\ UNCHANGED <<staged, checkpoint, readStatus, savedCheckpoint, savedValid>>

DoneStutter == /\ phase \in {"done", "blocked"}
               /\ UNCHANGED vars

Next == Produce \/ CommitWatermark \/ FlushCheckpoint \/ Crash
     \/ (\E outcome \in AcceptedReads \cup RejectedReads : ObserveRead(outcome))
     \/ Recover \/ RejectRead \/ Complete \/ DoneStutter

TypeOK == /\ durable \in 0..MaxChunks
          /\ staged \in 0..MaxChunks
          /\ checkpoint \in 0..MaxChunks
          /\ checkpointValid \in BOOLEAN
          /\ phase \in {"running", "crashed", "done", "blocked"}
          /\ lastKind \in {"init", "produce", "commit", "flush", "crash", "recover", "reject", "complete"}
          /\ lastBefore \in 0..MaxChunks
          /\ lastAfter \in 0..MaxChunks
          /\ readStatus \in AcceptedReads \cup RejectedReads \cup {"pending"}
          /\ savedCheckpoint \in 0..MaxChunks
          /\ savedValid \in BOOLEAN

DurableWatermarkNeverRollsBack == lastAfter >= lastBefore
RecoveryNeverStartsBeforeDurable == staged >= durable
CrashPreservesDurableTruth == lastKind = "crash" => lastAfter = lastBefore
TerminalClearsCheckpoint == phase = "done" => ~checkpointValid
UnknownObservationNeverExecutes == phase \in {"running", "done"} => readStatus \in AcceptedReads
BlockedPreservesCheckpoint == phase = "blocked" =>
  /\ checkpoint = savedCheckpoint
  /\ checkpointValid = savedValid
  /\ staged = durable

\* Fair local scheduling after an observation suffices to decide. No progress
\* claim is made while a read stays pending or about a remote stop provider.
ObservationSettles == [](phase = "crashed" /\ readStatus # "pending" => <>(phase # "crashed"))

\* Negated witnesses for the shared mutation runner; failure proves reachability.
NoExactResume == ~(lastKind = "recover" /\ readStatus = "exact")
NoAbsentResume == ~(lastKind = "recover" /\ readStatus = "absent")
NoStorageBlock == ~(phase = "blocked" /\ readStatus = "storage" /\ checkpointValid)
NoFenceBlock == ~(phase = "blocked" /\ readStatus = "fenced")
NoRunMismatchBlock == ~(phase = "blocked" /\ readStatus = "wrong-run")
NoThreadMismatchBlock == ~(phase = "blocked" /\ readStatus = "wrong-thread")
NoCancelledReadBlock == ~(phase = "blocked" /\ readStatus = "cancelled" /\ checkpointValid)

Spec == Init /\ [][Next]_vars /\ WF_vars(Recover) /\ WF_vars(RejectRead)
=============================================================================
