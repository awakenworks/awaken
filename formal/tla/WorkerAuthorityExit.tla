---------------------- MODULE WorkerAuthorityExit ----------------------
EXTENDS SessionRealizationAuthority

(* Compose the existing exact-slot/cohort model, not another Worker lease FSM.
   One heartbeat exits by ordinary proof loss or dependency unwind. Its single
   supervisor calls the same synchronous cohort fence before the first cleanup
   await, including while the outer Worker is gracefully draining. Explicit
   owner abort may abandon the task: it cannot manufacture closure/completion.
   OS process death, future Session admission and production execution grants
   remain outside this bounded selected-cohort proof. *)
VARIABLES exitPhase, exitKind
exitVars == <<exitPhase, exitKind>>
allExitVars == <<vars, exitVars>>
ExitInit == Init /\ exitPhase = "running" /\ exitKind = "none"

(* H1-H4: the future (including unwind) is consumed before one revocation.
   There is no await between observing heartbeat completion and cohort close. *)
HeartbeatExits(kind) ==
  /\ exitPhase = "running"
  /\ CloseWorker
  /\ exitKind' = kind /\ exitPhase' = "revoking"

(* Existing cleanup actions own exact owner selection and independent peer
   progress. Unknown/locked cleanup may remain pending indefinitely. *)
CompleteExit ==
  /\ exitPhase = "revoking" /\ cleaned /\ peerCleaned /\ ~ownerLocked
  /\ exitPhase' = "finished"
  /\ UNCHANGED <<vars, exitKind>>

AbortExit ==
  /\ exitPhase \in {"running", "revoking"} /\ exitPhase' = "abandoned"
  /\ UNCHANGED <<vars, exitKind>>

ExitNext == (SessionAndCleanupNext /\ UNCHANGED exitVars)
  \/ (\E kind \in {"returned", "panicked"} : HeartbeatExits(kind))
  \/ CompleteExit \/ AbortExit
ExitSpec == ExitInit /\ [][ExitNext]_allExitVars

ExitTypeOK == TypeOK
  /\ exitPhase \in {"running", "revoking", "finished", "abandoned"}
  /\ exitKind \in {"none", "returned", "panicked"}
HeartbeatExitFencesCohort == exitKind # "none" => workerClosed /\ ~peerLive
CompletionRequiresRevocation == exitPhase = "finished"
  => workerClosed /\ cleaned /\ peerCleaned /\ ~ownerLocked
(* Reachability witnesses exclude an inert proof without a panic or completion. *)
NoPanickedRevocationReached == exitKind # "panicked"
NoCompletedRevocationReached == exitPhase # "finished"
=============================================================================
