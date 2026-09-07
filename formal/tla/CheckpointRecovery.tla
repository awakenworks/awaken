----------------------- MODULE CheckpointRecovery -----------------------
EXTENDS Naturals, TLC

CONSTANT MaxChunks

VARIABLE durable, staged, checkpoint, checkpointValid, phase,
         lastKind, lastBefore, lastAfter, readStatus, savedCheckpoint, savedValid, recovery

vars == <<durable, staged, checkpoint, checkpointValid, phase,
          lastKind, lastBefore, lastAfter, readStatus, savedCheckpoint, savedValid, recovery>>

AcceptedReads == {"absent", "exact"}
RejectedReads == {"storage", "fenced", "wrong-run", "wrong-thread", "cancelled", "deadline"}
EffectEntries == {"execute", "resume", "handoff"}
ControlEntries == {"cancel", "stop"}
EvidencePermits == recovery.evidence = "terminated" \/
                  (recovery.evidence = "absent" /\ ~savedValid)

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
        /\ recovery = [entry |-> "execute", evidence |-> "absent", quiescent |-> TRUE, acknowledged |-> FALSE]

Produce == /\ phase = "running"
           /\ staged < MaxChunks
           /\ staged' = staged + 1
           /\ lastKind' = "produce"
           /\ lastBefore' = durable
           /\ lastAfter' = durable
           /\ UNCHANGED <<durable, checkpoint, checkpointValid, phase,
                          readStatus, savedCheckpoint, savedValid, recovery>>

CommitWatermark == /\ phase = "running"
                   /\ durable' = staged
                   /\ lastKind' = "commit"
                   /\ lastBefore' = durable
                   /\ lastAfter' = staged
                   /\ UNCHANGED <<staged, checkpoint, checkpointValid, phase,
                                  readStatus, savedCheckpoint, savedValid, recovery>>

FlushCheckpoint == /\ phase = "running"
                   /\ checkpoint' = staged
                   /\ checkpointValid' = TRUE
                   /\ lastKind' = "flush"
                   /\ lastBefore' = durable
                   /\ lastAfter' = durable
                   /\ UNCHANGED <<durable, staged, phase,
                                  readStatus, savedCheckpoint, savedValid, recovery>>

Crash == /\ phase = "running"
         /\ phase' = "crashed"
         /\ staged' = durable
         /\ lastKind' = "crash"
         /\ lastBefore' = durable
         /\ lastAfter' = durable
         /\ readStatus' = "pending"
         /\ savedCheckpoint' = checkpoint
         /\ savedValid' = checkpointValid
         /\ recovery' \in [entry : EffectEntries \cup ControlEntries,
                            evidence : {"absent", "terminated", "unknown", "corrupt"},
                            quiescent : {FALSE}, acknowledged : {FALSE}]
         /\ UNCHANGED <<durable, checkpoint, checkpointValid>>

\* Decision table: observed absence/exact identity permits existing recovery;
\* storage/fence/foreign identity/cancelled observation never permits effects.
\* A pending read is not absence. Predecessor evidence is an independent input,
\* including unknown/malformed history. This models consumption, not its truth.
ObserveRead(outcome) ==
  /\ phase = "crashed" /\ readStatus = "pending"
  /\ outcome \in RejectedReads \cup {IF checkpointValid THEN "exact" ELSE "absent"}
  /\ readStatus' = outcome
  /\ UNCHANGED <<durable, staged, checkpoint, checkpointValid, phase,
                 lastKind, lastBefore, lastAfter, savedCheckpoint, savedValid, recovery>>

\* Abstract finite observation timer, not a provider completion event. Fairness
\* assumes a yielding executor and a scheduled local timer; it cannot preempt
\* blocking synchronous code or confer authority on the abandoned operation.
ReadDeadline == ObserveRead("deadline")

Recover == /\ phase = "crashed"
           /\ readStatus \in AcceptedReads
           /\ EvidencePermits /\ recovery.entry \in EffectEntries
           /\ recovery' = [recovery EXCEPT !.quiescent = TRUE]
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
              /\ (readStatus \in RejectedReads \/ ~EvidencePermits)
              /\ phase' = "blocked"
              /\ lastKind' = "reject"
              /\ lastBefore' = durable
              /\ lastAfter' = durable
              /\ UNCHANGED <<durable, staged, checkpoint, checkpointValid,
                             readStatus, savedCheckpoint, savedValid, recovery>>

ControlReturn == /\ phase = "crashed"
                 /\ recovery.entry \in ControlEntries
                 /\ readStatus \in AcceptedReads /\ EvidencePermits
                 /\ phase' = "controlled"
                 /\ recovery' = [recovery EXCEPT !.quiescent = TRUE]
                 /\ lastKind' = "control"
                 /\ lastBefore' = durable /\ lastAfter' = durable
                 /\ UNCHANGED <<durable, staged, checkpoint, checkpointValid,
                                readStatus, savedCheckpoint, savedValid>>

Complete == /\ phase = "running"
            /\ phase' = "done"
            /\ durable' = staged
            /\ checkpointValid' = FALSE
            /\ lastKind' = "complete"
            /\ lastBefore' = durable
            /\ lastAfter' = staged
            /\ UNCHANGED <<staged, checkpoint, readStatus, savedCheckpoint, savedValid, recovery>>

\* The physical owner must observe the COMPLETE control operation, not merely
\* a backend cancel callback which can return before the recovery/terminal gate.
AcknowledgeControlReturn ==
  /\ phase = "controlled" /\ ~recovery.acknowledged
  /\ recovery' = [recovery EXCEPT !.acknowledged = TRUE]
  /\ UNCHANGED <<durable, staged, checkpoint, checkpointValid, phase, lastKind,
                 lastBefore, lastAfter, readStatus, savedCheckpoint, savedValid>>

DoneStutter == /\ phase \in {"done", "blocked", "controlled"}
               /\ UNCHANGED vars

Next == Produce \/ CommitWatermark \/ FlushCheckpoint \/ Crash
     \/ (\E outcome \in (AcceptedReads \cup RejectedReads) \ {"deadline"} : ObserveRead(outcome))
     \/ ReadDeadline \/ Recover \/ RejectRead \/ ControlReturn \/ AcknowledgeControlReturn \/ Complete \/ DoneStutter

TypeOK == /\ durable \in 0..MaxChunks
          /\ staged \in 0..MaxChunks
          /\ checkpoint \in 0..MaxChunks
          /\ checkpointValid \in BOOLEAN
          /\ phase \in {"running", "crashed", "done", "blocked", "controlled"}
          /\ lastKind \in {"init", "produce", "commit", "flush", "crash", "recover", "reject", "complete", "control"}
          /\ lastBefore \in 0..MaxChunks
          /\ lastAfter \in 0..MaxChunks
          /\ readStatus \in AcceptedReads \cup RejectedReads \cup {"pending"}
          /\ savedCheckpoint \in 0..MaxChunks
          /\ savedValid \in BOOLEAN
          /\ recovery \in [entry : EffectEntries \cup ControlEntries,
                            evidence : {"absent", "terminated", "unknown", "corrupt"},
                            quiescent : BOOLEAN, acknowledged : BOOLEAN]

DurableWatermarkNeverRollsBack == lastAfter >= lastBefore
RecoveryNeverStartsBeforeDurable == staged >= durable
CrashPreservesDurableTruth == lastKind = "crash" => lastAfter = lastBefore
TerminalClearsCheckpoint == phase = "done" => ~checkpointValid
UnknownObservationNeverExecutes == phase \in {"running", "done", "controlled"} => readStatus \in AcceptedReads
BlockedPreservesCheckpoint == phase \in {"blocked", "controlled"} =>
  /\ checkpoint = savedCheckpoint
  /\ checkpointValid = savedValid
  /\ staged = durable
UnknownPredecessorNeverExecutes == phase \in {"running", "done", "controlled"} =>
  recovery.evidence \notin {"unknown", "corrupt"}
RecoveredPartialRequiresEvidence == lastKind = "recover" /\ savedValid =>
  recovery.evidence = "terminated"
UncertainOutcomeVetoesPhysicalReturn == phase = "blocked" => ~recovery.quiescent
ControlReturnRequiresEvidence == phase = "controlled" => EvidencePermits
ControlAckRequiresObservedEvidence == recovery.acknowledged =>
  /\ phase = "controlled" /\ EvidencePermits /\ readStatus \in AcceptedReads

\* Decision progress includes an unresponsive store under fair local scheduling.
\* Neither property promises that an opaque remote operation terminates.
ObservationSettles == [](phase = "crashed" /\ readStatus # "pending" => <>(phase # "crashed"))
RecoveryReadSettles == [](phase = "crashed" => <>(phase # "crashed"))

\* Negated witnesses for the shared mutation runner; failure proves reachability.
NoExactResume == ~(lastKind = "recover" /\ readStatus = "exact")
NoAbsentResume == ~(lastKind = "recover" /\ readStatus = "absent")
NoStorageBlock == ~(phase = "blocked" /\ readStatus = "storage" /\ checkpointValid)
NoFenceBlock == ~(phase = "blocked" /\ readStatus = "fenced")
NoRunMismatchBlock == ~(phase = "blocked" /\ readStatus = "wrong-run")
NoThreadMismatchBlock == ~(phase = "blocked" /\ readStatus = "wrong-thread")
NoCancelledReadBlock == ~(phase = "blocked" /\ readStatus = "cancelled" /\ checkpointValid)
NoDeadlineReadBlock == ~(phase = "blocked" /\ readStatus = "deadline" /\ checkpointValid)
NoUnknownResumeBlock == ~(phase = "blocked" /\ recovery.entry = "resume" /\ recovery.evidence = "unknown")
NoUnknownHandoffBlock == ~(phase = "blocked" /\ recovery.entry = "handoff" /\ recovery.evidence = "unknown")
NoUnknownCancelBlock == ~(phase = "blocked" /\ recovery.entry = "cancel" /\ recovery.evidence = "unknown")
NoUnknownStopBlock == ~(phase = "blocked" /\ recovery.entry = "stop" /\ recovery.evidence = "unknown")
NoKnownControlReturn == ~(phase = "controlled" /\ recovery.evidence = "terminated")
NoControlAckReached == ~recovery.acknowledged

Spec == Init /\ [][Next]_vars /\ WF_vars(Recover) /\ WF_vars(RejectRead) /\ WF_vars(ControlReturn) /\ WF_vars(ReadDeadline)
=============================================================================
