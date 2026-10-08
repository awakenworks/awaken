-------------------- MODULE SessionExecutionReplacement --------------------
EXTENDS SessionExecutionBinding

(* Composition of bounded execution objects, not rebinding one authority. The
   original BindingSpec still proves write-once capture over its entire life.
   Reuse its current-binding predicate for the successor and its Worker/Session
   renewal, close and generation-install actions. Two callers share the Host
   lifecycle lock; cancellation releases the caller, never the physical owner.
   One physical root, one predecessor tree, one cancellation per caller.
   The default two-generation profile is unchanged; the four-generation
   profile also replaces fresh/ready successors. An append-only capture map
   retains every execution identity; nextCapture only selects the current one.
   The original owner may be Resident, committed Candidate, uncommitted
   Candidate, or Candidate whose Create committed but response was lost.
   The successor's root commit and readback are separate steps: cancellation
   loses neither the owner nor the committed receipt and replay writes once.
   Immutable physical generation is distinct from per-Adopt root receipts.
   copyWriter abstracts one complete Copy edit by its execution generation.
   Live adoption retains that local image, not a fresh snapshot of the source.
   This is not MemoryCAS writeback, partial I/O, FUSE or a content-hash proof;
   the real MC-M processes check exact bytes through the retained mount.
   Observer truth, exact root/Memory identity and atomic Host publication are
   assumptions discharged separately by executable tests, not Rust refinement.
   Unknown evidence has no liveness guarantee. Enabledness below forbids a
   vacuously safe implementation which simply never admits the new scope. *)
VARIABLES phase, caller, cancelled, tree, observation, nextCapture, newEffect, badNewEffect, originalPhase
replacementVars == <<phase, caller, cancelled, tree, observation, nextCapture, newEffect, badNewEffect, originalPhase>>
VARIABLES rootReceipt, observedReceipt, rootWrites, cancelledAfterCommit, badRootCommit,
          captures, cancelledFresh, cancelledCommitted
receiptVars == <<rootReceipt, observedReceipt, rootWrites, cancelledAfterCommit, badRootCommit,
                captures, cancelledFresh, cancelledCommitted>>
VARIABLE copyWriter
replacementAll == <<bindingAll, replacementVars, receiptVars, copyWriter>>
FourGenerations == 4
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
  /\ rootWrites = [g \in 2..GenerationLimit |-> 0]
  /\ captures = [g \in 2..GenerationLimit |-> <<0, 0, 0>>]
  /\ cancelledFresh = {} /\ cancelledCommitted = {}
  /\ cancelledAfterCommit = FALSE /\ badRootCommit = FALSE
  /\ tree = "absent" /\ observation = "unknown"
  /\ nextCapture = <<0, 0, 0>> /\ newEffect = FALSE /\ badNewEffect = FALSE
  /\ copyWriter = 0

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
       (phase = "fresh" /\ rootReceipt = environmentIdentity /\ observedReceipt # environmentIdentity))
  /\ cancelledFresh' = cancelledFresh \cup (IF phase = "fresh" THEN {environmentIdentity} ELSE {})
  /\ cancelledCommitted' = cancelledCommitted \cup
       (IF phase = "fresh" /\ rootReceipt = environmentIdentity THEN {environmentIdentity} ELSE {})
  /\ UNCHANGED <<rootReceipt, observedReceipt, rootWrites, badRootCommit, captures>>
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
  /\ phase' = "fresh" /\ environmentIdentity' = lease[1] /\ environmentPhase' = "candidate"
  /\ nextCapture' = <<slotIdentity, lease[1], lease[1]>>
  /\ captures' = [captures EXCEPT ![lease[1]] = nextCapture']
  /\ observedReceipt' = 0
  /\ UNCHANGED <<rootReceipt, rootWrites, cancelledAfterCommit, badRootCommit, cancelledFresh, cancelledCommitted>>
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       sessionExpired, effectHeld, effectChecked, badEffect, effects, effectPhases,
       weakStarted, deploymentReady, registeredWorker, providerSource, providerPhase,
       memoryVars, caller, cancelled, tree, observation, newEffect, badNewEffect>>
Publish == /\ caller # 0 /\ phase = "fresh" /\ Successor!CurrentBinding
  /\ observedReceipt = environmentIdentity
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
  /\ rootReceipt # environmentIdentity /\ rootWrites[environmentIdentity] < 2
  /\ rootReceipt' = environmentIdentity
  /\ rootWrites' = [rootWrites EXCEPT ![environmentIdentity] = @ + 1]
  /\ badRootCommit' = (~RootCurrent \/ projection # "Live" \/ sessionExpired
       \/ nextCapture # <<slotIdentity, lease[1], environmentIdentity>>
       \/ environmentPhase # "candidate")
  /\ UNCHANGED <<bindingAll, replacementVars, observedReceipt, cancelledAfterCommit,
                captures, cancelledFresh, cancelledCommitted>>
ReadReceipt == /\ caller # 0 /\ phase = "fresh" /\ Successor!CurrentBinding
  /\ rootReceipt = environmentIdentity /\ observedReceipt # environmentIdentity
  /\ observedReceipt' = environmentIdentity
  /\ UNCHANGED <<bindingAll, replacementVars, rootReceipt, rootWrites, cancelledAfterCommit, badRootCommit,
                captures, cancelledFresh, cancelledCommitted>>
(* A new Control claim requests replacement after a caller cancelled or a
   Resident was published. No identity is rebound: Retire/Stop/Install/Adopt
   remain the sole owner transitions. A live predecessor is conservative even
   if no modeled execution occurred. Unknown observation never enables Install. *)
ReplaceSuccessor == /\ caller = 0 /\ phase \in {"fresh", "ready"}
  /\ environmentIdentity < GenerationLimit
  /\ phase' = "old" /\ tree' = "live" /\ observation' = "unknown"
  /\ newEffect' = FALSE
  /\ UNCHANGED <<bindingAll, caller, cancelled, nextCapture, badNewEffect>>
(* Bind the same receipt frame before enumerating the control alternatives.
   Conjunction is commutative: this changes neither actions nor reachable
   states. TLC otherwise repeatedly binds the frame after each alternative.
   Cancel/CommitRoot/ReadReceipt stay outside because they may write receipts. *)
ReplacementControlStep == ((UNCHANGED receiptVars /\ (Setup \/ Authority
  \/ (\E c \in Callers : Acquire(c)) \/ Retire \/ Stop
  \/ ObserveAbsent \/ ObserveUnknown \/ Install \/ Publish \/ ReplaceSuccessor)
  ) \/ Cancel \/ CommitRoot \/ ReadReceipt)
ReplacementNext == /\ UNCHANGED originalPhase
  /\ ((ReplacementControlStep /\ UNCHANGED copyWriter)
  \/ (Adopt /\ UNCHANGED copyWriter)
  \/ (((StartOriginal /\ copyWriter' = 1)
       \/ (ExecuteSuccessor /\ copyWriter' = environmentIdentity)) /\ UNCHANGED receiptVars))
ReplacementSpec == ReplacementInit /\ [][ReplacementNext]_replacementAll

ReplacementTypeOK == BindingTypeOK
  /\ copyWriter \in 0..GenerationLimit
  /\ phase \in {"initial", "old", "retiring", "adoptable", "fresh", "ready"}
  /\ caller \in 0..2 /\ cancelled \subseteq Callers
  /\ originalPhase \in {"candidate", "resident", "uncommitted", "lost"}
  /\ rootReceipt \in 0..GenerationLimit /\ observedReceipt \in 0..GenerationLimit
  /\ rootWrites \in [2..GenerationLimit -> 0..2]
  /\ captures \in [2..GenerationLimit -> (0..2) \X (0..GenerationLimit) \X (0..EnvironmentLimit)]
  /\ cancelledFresh \subseteq 2..GenerationLimit /\ cancelledCommitted \subseteq cancelledFresh
  /\ cancelledAfterCommit \in BOOLEAN /\ badRootCommit \in BOOLEAN
  /\ tree \in {"live", "absent"} /\ observation \in {"unknown", "absent"}
  /\ nextCapture \in (0..2) \X (0..GenerationLimit) \X (0..EnvironmentLimit) /\ newEffect \in BOOLEAN /\ badNewEffect \in BOOLEAN
SuccessorCommitsOnlyCurrent == ~badNewEffect
InstallRequiresQuiescence == phase \in {"adoptable", "fresh", "ready"} => tree = "absent" /\ observation = "absent"
OriginalCannotReopen == phase \in {"retiring", "adoptable", "fresh", "ready"} => ~CurrentBinding
FreshCaptureMatchesOwner == phase \in {"fresh", "ready"} =>
  nextCapture = <<slotIdentity, lease[1], environmentIdentity>>
RetiredCapturesCannotAuthorize == \A g \in 2..(environmentIdentity - 1) :
  captures[g] # <<slotIdentity, lease[1], environmentIdentity>>
FreshExecutionEnabled == (phase \in {"fresh", "ready"} /\ RootCurrent
  /\ projection = "Live" /\ ~sessionExpired /\ ~newEffect) => ENABLED ExecuteSuccessor
CommittedOwnerRetirementEnabled == (caller # 0 /\ phase = "old") => ENABLED Retire
PublicationHasReadback == phase = "ready" => observedReceipt = environmentIdentity /\ rootReceipt = environmentIdentity
ReadbackNamesCommittedRoot == observedReceipt > 1 => rootWrites[observedReceipt] = 1
ReadbackIsInvocationSpecific == phase = "fresh" => observedReceipt \in {0, environmentIdentity}
RootReceiptWrittenOnce == \A g \in 2..GenerationLimit : rootWrites[g] <= 1
RootCommitIsCurrent == ~badRootCommit
FreshRootCommitEnabled == (caller # 0 /\ phase = "fresh" /\ RootCurrent
  /\ projection = "Live" /\ ~sessionExpired /\ rootReceipt # environmentIdentity) => ENABLED CommitRoot
CancellationRetainsOwner == [][Cancel => UNCHANGED <<phase, environmentIdentity, nextCapture, tree>>]_replacementAll
SuccessorCaptureIsWriteOnce == [][\A g \in 2..GenerationLimit :
  captures[g] # <<0, 0, 0>> => UNCHANGED captures[g]]_replacementAll
CaptureSelectsImmutableObject == phase \in {"fresh", "ready"} => captures[environmentIdentity] = nextCapture
CopyNeverRewinds == [][copyWriter' >= copyWriter]_replacementAll
SuccessorMemoryIsCurrent == newEffect => copyWriter = environmentIdentity
NoSuccessorReached == ~newEffect
NoCancelledRecoveryReached == ~(newEffect /\ cancelled = Callers)
NoCandidateSuccessorReached == ~(newEffect /\ originalPhase = "candidate")
NoUncommittedRecoveryReached == ~(phase = "ready" /\ originalPhase = "uncommitted")
NoLostResponseRecoveryReached == ~(phase = "ready" /\ originalPhase = "lost")
NoPostCommitCancellationRecoveryReached == ~(phase = "ready" /\ cancelledAfterCommit)
(* Positive witnesses need one feasible path, not universal liveness. Keep
   authority stable only for their existential search; neither safety profile
   nor semantic mutant checks use this constraint. *)
RecoveryWitnessScope == bootNow = 0 /\ rootPhase # "Closed" /\ projection = "Live"
  /\ lease[2] = 1 /\ request = NoSnapshot /\ originalPhase = "uncommitted"
NoMultiRecovery00 == ~(phase = "ready" /\ environmentIdentity = 4 /\ cancelledFresh = {2, 3} /\ cancelledCommitted = {})
NoMultiRecovery01 == ~(phase = "ready" /\ environmentIdentity = 4 /\ cancelledFresh = {2, 3} /\ cancelledCommitted = {3})
NoMultiRecovery10 == ~(phase = "ready" /\ environmentIdentity = 4 /\ cancelledFresh = {2, 3} /\ cancelledCommitted = {2})
NoMultiRecovery11 == ~(phase = "ready" /\ environmentIdentity = 4 /\ cancelledFresh = {2, 3} /\ cancelledCommitted = {2, 3})
NoMultiMemoryRecovery == ~(phase = "ready" /\ environmentIdentity = 4
  /\ newEffect /\ copyWriter = 4 /\ cancelledFresh = {2, 3})
=============================================================================
