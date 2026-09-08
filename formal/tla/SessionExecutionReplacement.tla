-------------------- MODULE SessionExecutionReplacement --------------------
EXTENDS SessionExecutionBinding

(* Composition of TWO execution objects, not rebinding one authority. The
   original BindingSpec still proves write-once capture over its entire life.
   Reuse its current-binding predicate for the successor and its Worker/Session
   renewal, close and generation-install actions. Two callers share the Host
   lifecycle lock; cancellation releases the caller, never the physical owner.
   One old tree, two generations, one successor, one cancellation per caller.
   The original owner may be Resident, committed Candidate, uncommitted
   Candidate, or Candidate whose Create committed but response was lost.
   The successor's root commit and readback are separate steps: cancellation
   loses neither the owner nor the committed receipt and replay writes once.
   A second replacement after the successor becomes Candidate is not modeled.
   Observer truth, exact root/Memory identity and atomic Host publication are
   assumptions discharged separately by executable tests, not Rust refinement.
   Unknown evidence has no liveness guarantee. Enabledness below forbids a
   vacuously safe implementation which simply never admits the new scope. *)
VARIABLES phase, caller, cancelled, tree, observation, nextCapture, newEffect, badNewEffect, originalPhase
replacementVars == <<phase, caller, cancelled, tree, observation, nextCapture, newEffect, badNewEffect, originalPhase>>
VARIABLES rootReceipt, observedReceipt, rootWrites, cancelledAfterCommit, badRootCommit
receiptVars == <<rootReceipt, observedReceipt, rootWrites, cancelledAfterCommit, badRootCommit>>
replacementAll == <<bindingAll, replacementVars, receiptVars>>
OriginalEnvironmentPhase == IF originalPhase = "resident" THEN "resident" ELSE "candidate"
Successor == INSTANCE SessionExecutionBinding
  WITH boundSlot <- nextCapture[1], boundGeneration <- nextCapture[2],
       boundEnvironment <- nextCapture[3]

ReplacementInit == BindingInit
  /\ registeredWorker = TRUE /\ providerSource = FALSE /\ providerPhase = "adoption"
  /\ phase = "initial" /\ caller = 0 /\ cancelled = {}
  /\ originalPhase \in {"candidate", "resident", "uncommitted", "lost"}
  /\ rootReceipt = (IF originalPhase = "uncommitted" THEN 0 ELSE 1)
  /\ observedReceipt = (IF originalPhase \in {"candidate", "resident"} THEN 1 ELSE 0)
  /\ rootWrites = 0 /\ cancelledAfterCommit = FALSE /\ badRootCommit = FALSE
  /\ tree = "absent" /\ observation = "unknown"
  /\ nextCapture = <<0, 0, 0>> /\ newEffect = FALSE /\ badNewEffect = FALSE

Setup == /\ phase = "initial"
  /\ (ProbeReady \/ ((Select \/ Bind \/
      (EnvironmentChanges /\ environmentIdentity' = 1 /\ environmentPhase' = OriginalEnvironmentPhase))
      /\ UNCHANGED deploymentReady))
  /\ UNCHANGED <<registeredWorker, providerSource, providerPhase, memoryVars, replacementVars>>
StartOriginal == /\ phase = "initial" /\ CurrentBinding /\ environmentPhase = OriginalEnvironmentPhase
  /\ phase' = "old" /\ tree' = "live"
  /\ UNCHANGED <<bindingAll, caller, cancelled, observation, nextCapture, newEffect, badNewEffect>>

Authority == (RequestHeartbeat \/ ApplyHeartbeat \/ CloseRoot \/ Tick
    \/ RenewProjection \/ ((SendRenewal \/ BeginRevoke(1)) /\ UNCHANGED rootVars))
  /\ UNCHANGED <<bindingVars, deploymentReady, registeredWorker, providerSource, providerPhase,
                  memoryVars, replacementVars>>
Acquire(c) == /\ caller = 0 /\ phase \in {"old", "retiring", "adoptable", "fresh"}
  /\ caller' = c
  /\ UNCHANGED <<bindingAll, phase, cancelled, tree, observation, nextCapture, newEffect, badNewEffect>>
Cancel == /\ caller # 0 /\ caller \notin cancelled
  /\ cancelled' = cancelled \cup {caller} /\ caller' = 0
  /\ cancelledAfterCommit' = (cancelledAfterCommit \/
       (phase = "fresh" /\ rootReceipt = 2 /\ observedReceipt # 2))
  /\ UNCHANGED <<rootReceipt, observedReceipt, rootWrites, badRootCommit>>
  /\ UNCHANGED <<bindingAll, phase, tree, observation, nextCapture, newEffect, badNewEffect>>
Retire == /\ caller # 0 /\ phase = "old"
  /\ phase' = "retiring" /\ environmentPhase' = "retiring"
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, sessionExpired, effectHeld, effectChecked, badEffect,
       effects, effectPhases, weakStarted, deploymentReady, registeredWorker, providerSource,
       providerPhase, memoryVars, caller, cancelled, tree, observation, nextCapture, newEffect, badNewEffect>>
Stop == /\ phase = "retiring" /\ tree = "live" /\ tree' = "absent"
  /\ UNCHANGED <<bindingAll, phase, caller, cancelled, observation, nextCapture, newEffect, badNewEffect>>
ObserveAbsent == /\ phase = "retiring" /\ tree = "absent"
  /\ observation' = "absent"
  /\ UNCHANGED <<bindingAll, phase, caller, cancelled, tree, nextCapture, newEffect, badNewEffect>>
ObserveUnknown == /\ phase = "retiring" /\ observation' = "unknown"
  /\ UNCHANGED <<bindingAll, phase, caller, cancelled, tree, nextCapture, newEffect, badNewEffect>>
Install == /\ caller # 0 /\ phase = "retiring" /\ observation = "absent"
  /\ tree = "absent" /\ InstallReplacement /\ phase' = "adoptable"
  /\ UNCHANGED <<bindingVars, deploymentReady, registeredWorker, providerSource, providerPhase,
       memoryVars, caller, cancelled, tree, observation, nextCapture, newEffect, badNewEffect>>
Adopt == /\ caller # 0 /\ phase = "adoptable"
  /\ ExecutionRootCurrent /\ projection = "Live" /\ ~sessionExpired
  /\ phase' = "fresh" /\ environmentIdentity' = 2 /\ environmentPhase' = "candidate"
  /\ nextCapture' = <<slotIdentity, lease[1], 2>>
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       sessionExpired, effectHeld, effectChecked, badEffect, effects, effectPhases,
       weakStarted, deploymentReady, registeredWorker, providerSource, providerPhase,
       memoryVars, caller, cancelled, tree, observation, newEffect, badNewEffect>>
Publish == /\ caller # 0 /\ phase = "fresh" /\ Successor!CurrentBinding
  /\ observedReceipt = 2
  /\ phase' = "ready" /\ environmentPhase' = "resident" /\ caller' = 0
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, sessionExpired, effectHeld, effectChecked, badEffect, effects,
       effectPhases, weakStarted, deploymentReady, registeredWorker, providerSource,
       providerPhase, memoryVars, cancelled, tree, observation, nextCapture, newEffect, badNewEffect>>
ExecuteSuccessor == /\ phase \in {"fresh", "ready"} /\ Successor!CurrentBinding
  /\ ~newEffect /\ newEffect' = TRUE
  /\ badNewEffect' = (~RootCurrent \/ projection # "Live" \/ sessionExpired
       \/ nextCapture # <<slotIdentity, lease[1], environmentIdentity>>
       \/ environmentPhase \notin {"candidate", "resident"}
       \/ tree # "absent" \/ observation # "absent")
  /\ UNCHANGED <<bindingAll, phase, caller, cancelled, tree, observation, nextCapture>>
CommitRoot == /\ caller # 0 /\ phase = "fresh" /\ Successor!CurrentBinding
  /\ rootReceipt # 2 /\ rootWrites < 2
  /\ rootReceipt' = 2 /\ rootWrites' = rootWrites + 1
  /\ badRootCommit' = (~RootCurrent \/ projection # "Live" \/ sessionExpired
       \/ nextCapture # <<slotIdentity, lease[1], environmentIdentity>>
       \/ environmentPhase # "candidate")
  /\ UNCHANGED <<bindingAll, replacementVars, observedReceipt, cancelledAfterCommit>>
ReadReceipt == /\ caller # 0 /\ phase = "fresh" /\ Successor!CurrentBinding
  /\ rootReceipt = 2 /\ observedReceipt # 2 /\ observedReceipt' = 2
  /\ UNCHANGED <<bindingAll, replacementVars, rootReceipt, rootWrites, cancelledAfterCommit, badRootCommit>>
ReplacementNext == /\ UNCHANGED originalPhase
  /\ (((Setup \/ StartOriginal \/ Authority
  \/ (\E c \in Callers : Acquire(c)) \/ Retire \/ Stop
  \/ ObserveAbsent \/ ObserveUnknown \/ Install \/ Adopt \/ Publish \/ ExecuteSuccessor)
  /\ UNCHANGED receiptVars) \/ Cancel \/ CommitRoot \/ ReadReceipt)
ReplacementSpec == ReplacementInit /\ [][ReplacementNext]_replacementAll

ReplacementTypeOK == BindingTypeOK
  /\ phase \in {"initial", "old", "retiring", "adoptable", "fresh", "ready"}
  /\ caller \in 0..2 /\ cancelled \subseteq Callers
  /\ originalPhase \in {"candidate", "resident", "uncommitted", "lost"}
  /\ rootReceipt \in 0..2 /\ observedReceipt \in 0..2 /\ rootWrites \in 0..2
  /\ cancelledAfterCommit \in BOOLEAN /\ badRootCommit \in BOOLEAN
  /\ tree \in {"live", "absent"} /\ observation \in {"unknown", "absent"}
  /\ nextCapture \in (0..2) \X (0..2) \X (0..2) /\ newEffect \in BOOLEAN /\ badNewEffect \in BOOLEAN
SuccessorCommitsOnlyCurrent == ~badNewEffect
InstallRequiresQuiescence == lease[1] = 2 => tree = "absent" /\ observation = "absent"
OriginalCannotReopen == phase \in {"retiring", "adoptable", "fresh", "ready"} => ~CurrentBinding
FreshCaptureMatchesOwner == phase \in {"fresh", "ready"} =>
  nextCapture = <<slotIdentity, 2, environmentIdentity>>
FreshExecutionEnabled == (phase \in {"fresh", "ready"} /\ RootCurrent
  /\ projection = "Live" /\ ~sessionExpired /\ ~newEffect) => ENABLED ExecuteSuccessor
CommittedOwnerRetirementEnabled == (caller # 0 /\ phase = "old") => ENABLED Retire
PublicationHasReadback == phase = "ready" => observedReceipt = 2 /\ rootReceipt = 2
ReadbackNamesCommittedRoot == observedReceipt = 2 => rootReceipt = 2
RootReceiptWrittenOnce == rootWrites <= 1
RootCommitIsCurrent == ~badRootCommit
FreshRootCommitEnabled == (caller # 0 /\ phase = "fresh" /\ RootCurrent
  /\ projection = "Live" /\ ~sessionExpired /\ rootReceipt # 2) => ENABLED CommitRoot
CancellationRetainsOwner == [][Cancel => UNCHANGED <<phase, environmentIdentity, nextCapture, tree>>]_replacementAll
SuccessorCaptureIsWriteOnce == [][nextCapture # <<0, 0, 0>> => UNCHANGED nextCapture]_replacementAll
NoSuccessorReached == ~newEffect
NoCancelledRecoveryReached == ~(newEffect /\ cancelled = Callers)
NoCandidateSuccessorReached == ~(newEffect /\ originalPhase = "candidate")
NoUncommittedRecoveryReached == ~(phase = "ready" /\ originalPhase = "uncommitted")
NoLostResponseRecoveryReached == ~(phase = "ready" /\ originalPhase = "lost")
NoPostCommitCancellationRecoveryReached == ~(phase = "ready" /\ cancelledAfterCommit)
=============================================================================
