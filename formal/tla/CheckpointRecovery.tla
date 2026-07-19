----------------------- MODULE CheckpointRecovery -----------------------
EXTENDS Naturals, TLC

CONSTANT MaxChunks

VARIABLE durable, staged, checkpoint, checkpointValid, phase,
         lastKind, lastBefore, lastAfter

vars == <<durable, staged, checkpoint, checkpointValid, phase,
          lastKind, lastBefore, lastAfter>>

NatMax(a, b) == IF a >= b THEN a ELSE b

Init == /\ durable = 0
        /\ staged = 0
        /\ checkpoint = 0
        /\ checkpointValid = FALSE
        /\ phase = "running"
        /\ lastKind = "init"
        /\ lastBefore = 0
        /\ lastAfter = 0

Produce == /\ phase = "running"
           /\ staged < MaxChunks
           /\ staged' = staged + 1
           /\ lastKind' = "produce"
           /\ lastBefore' = durable
           /\ lastAfter' = durable
           /\ UNCHANGED <<durable, checkpoint, checkpointValid, phase>>

CommitWatermark == /\ phase = "running"
                   /\ durable' = staged
                   /\ lastKind' = "commit"
                   /\ lastBefore' = durable
                   /\ lastAfter' = staged
                   /\ UNCHANGED <<staged, checkpoint, checkpointValid, phase>>

FlushCheckpoint == /\ phase = "running"
                   /\ checkpoint' = staged
                   /\ checkpointValid' = TRUE
                   /\ lastKind' = "flush"
                   /\ lastBefore' = durable
                   /\ lastAfter' = durable
                   /\ UNCHANGED <<durable, staged, phase>>

Crash == /\ phase = "running"
         /\ phase' = "crashed"
         /\ staged' = durable
         /\ lastKind' = "crash"
         /\ lastBefore' = durable
         /\ lastAfter' = durable
         /\ UNCHANGED <<durable, checkpoint, checkpointValid>>

Recover == /\ phase = "crashed"
           /\ phase' = "running"
           /\ staged' = IF checkpointValid
                            THEN NatMax(durable, checkpoint)
                            ELSE durable
           /\ lastKind' = "recover"
           /\ lastBefore' = durable
           /\ lastAfter' = durable
           /\ UNCHANGED <<durable, checkpoint, checkpointValid>>

Complete == /\ phase = "running"
            /\ phase' = "done"
            /\ durable' = staged
            /\ checkpointValid' = FALSE
            /\ lastKind' = "complete"
            /\ lastBefore' = durable
            /\ lastAfter' = staged
            /\ UNCHANGED <<staged, checkpoint>>

DoneStutter == /\ phase = "done"
               /\ UNCHANGED vars

Next == Produce \/ CommitWatermark \/ FlushCheckpoint \/ Crash
     \/ Recover \/ Complete \/ DoneStutter

TypeOK == /\ durable \in 0..MaxChunks
          /\ staged \in 0..MaxChunks
          /\ checkpoint \in 0..MaxChunks
          /\ checkpointValid \in BOOLEAN
          /\ phase \in {"running", "crashed", "done"}
          /\ lastKind \in {"init", "produce", "commit", "flush", "crash", "recover", "complete"}
          /\ lastBefore \in 0..MaxChunks
          /\ lastAfter \in 0..MaxChunks

DurableWatermarkNeverRollsBack == lastAfter >= lastBefore
RecoveryNeverStartsBeforeDurable == staged >= durable
CrashPreservesDurableTruth == lastKind = "crash" => lastAfter = lastBefore
TerminalClearsCheckpoint == phase = "done" => ~checkpointValid

Spec == Init /\ [][Next]_vars
=============================================================================
