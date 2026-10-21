-------------------- MODULE RetainedWorkerRetirement --------------------
EXTENDS FiniteSets

(* Selected, already-draining cohort. Original source/UID observations and
   closed business-work admission are premises qualified by their owners.
   An owned cleanup lease may remain live: forced stop is safe only because
   bytes are independent OR preparation is durable. Actual exit, Create closure
   and lease succession remain separate before the SAME roots move to a new
   Actor. This bounded model does not prove APIs, clocks or Rust refinement. *)
CONSTANTS Sources, IndependentSources, KnownSources, InitiallyPrepared, NeedsNewCode, SessionTerminal
ASSUME /\ Sources # {} /\ IndependentSources \subseteq Sources
       /\ KnownSources \subseteq Sources /\ InitiallyPrepared \subseteq Sources
       /\ NeedsNewCode \subseteq Sources
ASSUME /\ SessionTerminal \in BOOLEAN /\ (~SessionTerminal => InitiallyPrepared = {})
VARIABLES leaseLive, permit, oldStopped, oldExited, createClosed, rootOwner,
          prepared, present, cleanupAck
actorVars == <<leaseLive, permit, oldStopped, oldExited, createClosed, rootOwner>>
sourceVars == <<prepared, present, cleanupAck>>
vars == <<actorVars, sourceVars>>
Init == /\ leaseLive = TRUE /\ permit = FALSE
        /\ oldStopped = FALSE /\ oldExited = FALSE /\ createClosed = FALSE
        /\ rootOwner = "original"
        /\ prepared = [s \in Sources |-> s \in InitiallyPrepared]
        /\ present = [s \in Sources |-> TRUE] /\ cleanupAck = FALSE

(* Normal owned cleanup keeps its existing renewal protocol. No run-grace timer
   truncates it. Once the original process has exited its lease expires through
   the existing independent authority boundary, not by this retirement read. *)
RenewOld == /\ ~oldStopped /\ leaseLive' = TRUE
            /\ UNCHANGED <<permit, oldStopped, oldExited, createClosed, rootOwner, sourceVars>>
ExpireOldLease == /\ oldExited /\ leaseLive /\ leaseLive' = FALSE
                  /\ UNCHANGED <<permit, oldStopped, oldExited, createClosed, rootOwner, sourceVars>>
SourceRecoverable(s) == s \in KnownSources
                       /\ (prepared[s] \/ (s \in IndependentSources /\ present[s]))
Qualify == /\ ~oldStopped /\ ~permit
           /\ \A s \in Sources : SourceRecoverable(s)
           /\ permit' = TRUE
           /\ UNCHANGED <<leaseLive, oldStopped, oldExited, createClosed, rootOwner, sourceVars>>
StopOld == /\ permit /\ ~oldStopped /\ oldStopped' = TRUE
           /\ UNCHANGED <<leaseLive, permit, oldExited, createClosed, rootOwner, sourceVars>>
ObserveExit == /\ oldStopped /\ ~oldExited /\ oldExited' = TRUE
               (* Actor-local buffers can disappear at exit. They must already
                  be prepared; independent source data survives this stop. *)
               /\ present' = [s \in Sources |->
                     IF s \in IndependentSources THEN present[s] ELSE FALSE]
               /\ UNCHANGED <<leaseLive, permit, oldStopped, createClosed,
                               rootOwner, prepared, cleanupAck>>
SealCreate == /\ oldExited /\ ~createClosed /\ createClosed' = TRUE
              /\ UNCHANGED <<leaseLive, permit, oldStopped, oldExited, rootOwner, sourceVars>>
ClaimNew == /\ oldExited /\ createClosed /\ ~leaseLive /\ rootOwner = "original"
            /\ rootOwner' = "successor"
            /\ UNCHANGED <<leaseLive, permit, oldStopped, oldExited, createClosed, sourceVars>>
Prepare(s) == /\ SessionTerminal /\ s \in Sources /\ ~prepared[s] /\ present[s]
              /\ \/ rootOwner = "successor"
                 \/ (~oldStopped /\ leaseLive /\ s \notin NeedsNewCode)
              /\ prepared' = [prepared EXCEPT ![s] = TRUE]
              /\ UNCHANGED <<actorVars, present, cleanupAck>>
Dispose(s) == /\ SessionTerminal /\ rootOwner = "successor" /\ s \in Sources /\ present[s]
              /\ \A source \in Sources : prepared[source]
              /\ present' = [present EXCEPT ![s] = FALSE]
              /\ UNCHANGED <<actorVars, prepared, cleanupAck>>
Acknowledge == /\ SessionTerminal /\ rootOwner = "successor" /\ ~cleanupAck
               /\ \A s \in Sources : prepared[s] /\ ~present[s]
               /\ cleanupAck' = TRUE
               /\ UNCHANGED <<actorVars, prepared, present>>
Next == RenewOld \/ ExpireOldLease \/ Qualify \/ StopOld \/ ObserveExit
        \/ SealCreate \/ ClaimNew
        \/ (\E s \in Sources : Prepare(s) \/ Dispose(s)) \/ Acknowledge
Fair == WF_vars(Qualify) /\ WF_vars(StopOld) /\ WF_vars(ObserveExit)
        /\ WF_vars(ExpireOldLease) /\ WF_vars(SealCreate) /\ WF_vars(ClaimNew)
        /\ (\A s \in Sources : WF_vars(Prepare(s)) /\ WF_vars(Dispose(s)))
        /\ WF_vars(Acknowledge)
Spec == Init /\ [][Next]_vars /\ Fair
TypeOK == /\ leaseLive \in BOOLEAN /\ permit \in BOOLEAN
          /\ oldStopped \in BOOLEAN /\ oldExited \in BOOLEAN /\ createClosed \in BOOLEAN
          /\ rootOwner \in {"original", "successor"}
          /\ prepared \in [Sources -> BOOLEAN] /\ present \in [Sources -> BOOLEAN]
          /\ cleanupAck \in BOOLEAN
UnpreparedSourceSurvivesStop == oldExited => \A s \in Sources : ~prepared[s] => present[s]
SuccessorRequiresExitAndCreateClosure == rootOwner = "successor"
    => oldExited /\ createClosed /\ ~leaseLive
RetirementIsNotCleanupAck == cleanupAck => rootOwner = "successor" /\
    (\A s \in Sources : prepared[s] /\ ~present[s])
QualifiedRetirementEventuallyCompletes ==
    (SessionTerminal /\ KnownSources = Sources /\
       (\A s \in Sources : s \in IndependentSources \/ s \in InitiallyPrepared))
       => <>cleanupAck
QualifiedRetirementEventuallyTransfers ==
    (KnownSources = Sources /\
       (\A s \in Sources : s \in IndependentSources \/ s \in InitiallyPrepared))
       => <>(rootOwner = "successor")
LiveSourcesNeverCleaned == ~SessionTerminal =>
    /\ ~cleanupAck /\ (\A s \in Sources : present[s] /\ ~prepared[s])
NoRetirementReached == ~oldExited
NoCleanupReached == ~cleanupAck
=============================================================================
