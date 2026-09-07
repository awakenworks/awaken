-------------------- MODULE SessionExecutionBinding --------------------
EXTENDS WorkerExecutionAuthority

(* Compose the existing Worker receipt and Session slot kernel, rather than
   invent another lease. One scope, two slot/environment identities and two
   Session generations. Root -> slot locks span the callback; the split
   check/commit exposes a lock-release mutant. Time within a synchronous grant
   is abstracted atomically here; LocalProcessDeadline/Boundary own physical
   cutoff and callback-delay bounds. This is not an OS or Rust refinement proof.
   The separate Session-expiry bit does NOT equate wall time with BOOTTIME.
   registeredWorker is fixed at initialization: the deployment composition is
   trusted to require root proof for registered workers; a direct unregistered
   Host has only its real Session owner, never an invented infinite Worker
   receipt. This model does not prove that deployment classification or the
   new request-relative Session clock adapter is implemented correctly. *)
(* providerSource selects the shared pre-Environment effect authority: it captures
   the same slot/generation before preparation, but no Environment exists yet.
   A callback result is only a durable-fence projection, NOT asynchronous
   provider mutation authority. The check/commit here spans that projection
   under the Worker/slot locks. Backend expiry, clock conversion, operation-id
   bytes and actual I/O after return remain executable/adapter obligations.
   Memory acknowledgement additionally waits for its async mount lock BEFORE
   entering the same check/commit. memoryWait is 0=no pending wait, 1=waiting,
   2=mount lock acquired. memoryDrains is the cached process-local Copy ack:
   replay still needs current authority but never drains twice. This models
   the local acknowledgement only; MemoryCAS owns durable reconciliation and
   retention. No fairness/termination of an unavailable mount is assumed. *)
VARIABLES deploymentReady, registeredWorker, providerSource, providerPhase
physicalDisposal == providerPhase = "disposal"
coldReconstruction == providerPhase = "reconstruction"
restoredDisposal == providerPhase = "restore-disposal"
checkpointUpload == providerPhase = "checkpoint"
(* Reconstruction, preparation, checkpoint upload and disposal share one wait transition,
   not independent authorization protocols. providerPhase is fixed at initialization;
   this proves admission within that phase, not aggregate preparation ordering.
   Both use the same current-effect kernel after control/credential/removal
   waits. providerWait=1 is an admitted pending call, =2 its response. An
   admitted backend may finish after revocation; its response is NOT a grant
   for the next operation or a successful preparation receipt. This finite
   model abstracts one such wait and two effects, not backend commit fencing.
   Kubernetes compound cleanup maps each adjacent HTTP-response/next-effect
   pair to this SAME wait/check transition (disposal or restore-disposal).
   cleanup_tests.rs enumerates every response, including finalizer CAS retry,
   and checks the committed prefix plus fresh-successor replay. These adapter
   schedules connect the model obligation to real kube requests; they do not
   establish Rust refinement, API-server fencing, or controller liveness. *)
VARIABLES memoryWait, memoryDrains, providerWait
memoryVars == <<memoryWait, memoryDrains, providerWait>>
VARIABLES selection, boundSlot, boundGeneration, boundEnvironment,
          environmentIdentity, environmentPhase, sessionExpired,
          effectHeld, effectChecked, badEffect, effects, effectPhases, weakStarted
bindingVars == <<selection, boundSlot, boundGeneration, boundEnvironment,
    environmentIdentity, environmentPhase, sessionExpired, effectHeld,
    effectChecked, badEffect, effects, effectPhases, weakStarted>>
bindingAll == <<allVars, bindingVars, deploymentReady, registeredWorker, providerSource, providerPhase, memoryVars>>

BindingInit == RootInit
  /\ registeredWorker \in BOOLEAN
  /\ providerSource \in BOOLEAN
  /\ providerPhase \in {"preparation", "disposal", "reconstruction", "restore-disposal", "checkpoint"}
  /\ deploymentReady = FALSE
  /\ selection = "unset" /\ boundSlot = 0 /\ boundGeneration = 0 /\ boundEnvironment = 0
  /\ environmentIdentity = 1 /\ environmentPhase = "candidate" /\ sessionExpired = FALSE
  /\ effectHeld = FALSE /\ effectChecked = FALSE /\ badEffect = FALSE
  /\ effects = 0 /\ effectPhases = {} /\ weakStarted = FALSE
  /\ memoryWait = 0 /\ memoryDrains = 0
  /\ providerWait = 0

ExecutionRootCurrent == ~registeredWorker \/ RootCurrent
CurrentBinding == /\ ExecutionRootCurrent /\ projection = "Live" /\ ~sessionExpired
  /\ boundSlot = slotIdentity /\ boundGeneration = lease[1]
  /\ (providerSource \/
      (/\ selection = "scoped" /\ boundEnvironment = environmentIdentity
       /\ environmentPhase \in {"candidate", "resident"}))

Select == /\ selection = "unset" /\ deploymentReady /\ selection' = "scoped"
  /\ ~providerSource
  /\ UNCHANGED <<allVars, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, effectHeld,
       effectChecked, badEffect, effects, effectPhases, weakStarted>>
WeakSpawn == /\ selection \in {"unset", "weak"}
  /\ ~providerSource
  /\ selection' = "weak" /\ weakStarted' = TRUE
  /\ UNCHANGED <<allVars, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, effectHeld,
       effectChecked, badEffect, effects, effectPhases>>
Bind == /\ (providerSource \/ selection = "scoped") /\ boundSlot = 0 /\ projection = "Live"
  /\ boundSlot' = slotIdentity /\ boundGeneration' = lease[1]
  /\ boundEnvironment' = environmentIdentity
  /\ UNCHANGED <<allVars, selection, environmentIdentity, environmentPhase,
       sessionExpired, effectHeld, effectChecked, badEffect, effects, effectPhases, weakStarted>>
CheckEffect == /\ ~effectHeld /\ effects < 2 /\ CurrentBinding
  /\ memoryWait # 1
  /\ providerWait # 1
  /\ effectHeld' = TRUE /\ effectChecked' = TRUE
  /\ UNCHANGED memoryVars
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, badEffect, effects, effectPhases, weakStarted>>
CommitEffect == /\ effectHeld /\ effectChecked /\ effects < 2
  \* Independent receipt oracle: changing an admission guard must not also
  \* weaken the assertion against which that mutant is checked.
  /\ badEffect' = (badEffect \/ (registeredWorker /\ ~RootCurrent)
       \/ memoryWait = 1
       \/ providerWait = 1
       \/ projection # "Live" \/ sessionExpired
       \/ boundSlot # slotIdentity \/ boundGeneration # lease[1]
       \/ (~providerSource /\ (selection # "scoped"
           \/ boundEnvironment # environmentIdentity \/ environmentPhase = "retiring")))
  /\ effects' = effects + 1
  /\ effectPhases' = effectPhases \cup {IF memoryWait = 2 THEN
       (IF memoryDrains = 0 THEN "memory" ELSE "memory-replay")
       ELSE IF providerWait = 2 THEN
         (IF physicalDisposal THEN
          (IF "disposal" \in effectPhases THEN "disposal-replay" ELSE "disposal")
          ELSE IF coldReconstruction THEN
           (IF "reconstruction" \in effectPhases THEN "reconstruction-replay" ELSE "reconstruction")
          ELSE IF restoredDisposal THEN
           (IF "restore-disposal" \in effectPhases THEN "restore-disposal-replay" ELSE "restore-disposal")
          ELSE IF checkpointUpload THEN
           (IF "checkpoint" \in effectPhases THEN "checkpoint-replay" ELSE "checkpoint")
          ELSE "preparation")
       ELSE IF providerSource THEN "provider" ELSE environmentPhase}
  /\ memoryDrains' = (IF memoryWait = 2 THEN 1 ELSE memoryDrains)
  /\ memoryWait' = 0
  /\ providerWait' = 0
  /\ effectHeld' = FALSE /\ effectChecked' = FALSE
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, weakStarted>>
CancelEffect == /\ effectHeld /\ effectHeld' = FALSE /\ effectChecked' = FALSE
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, badEffect, effects, effectPhases, weakStarted>>

(* Existing authority transition owners; no shadow renew/revoke implementation.
   Peer cleanup/fresh Session counts are irrelevant to this one scope and stay
   fixed; their full interleavings remain in WorkerExecutionAuthority. *)
AuthorityChanges == ((registeredWorker /\
      (RequestHeartbeat \/ ApplyHeartbeat \/ CloseRoot \/ Tick
       \/ InstallReplacement \/ RenewProjection))
    \/ ((SendRenewal \/ BeginRevoke(1) \/ RecreateClosedSlot
          \/ (~registeredWorker /\ (Replace \/ ApplyRenewal))
          \/ (ExecutionRootCurrent /\ RecreateSlot)) /\ UNCHANGED rootVars))
  /\ ~effectHeld /\ UNCHANGED bindingVars
EnvironmentChanges == /\ ~effectHeld
  /\ \/ (/\ ~providerSource /\ environmentPhase = "candidate" /\ environmentPhase' = "resident"
          /\ UNCHANGED <<environmentIdentity, sessionExpired>>)
     \/ (/\ ~providerSource /\ environmentPhase \in {"candidate", "resident"}
          /\ environmentPhase' = "retiring" /\ UNCHANGED <<environmentIdentity, sessionExpired>>)
     \/ (/\ ~providerSource /\ environmentIdentity = 1 /\ environmentIdentity' = 2
          /\ UNCHANGED <<environmentPhase, sessionExpired>>)
     \/ (/\ ~sessionExpired /\ sessionExpired' = TRUE
          /\ UNCHANGED <<environmentIdentity, environmentPhase>>)
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       effectHeld, effectChecked, badEffect, effects, effectPhases, weakStarted>>

\* Startup is sequential/owned before serving. Failure/stall leaves readiness
\* false; only a complete private-handoff/tree-exit probe permits selection.
\* The probe's OS truth is assumed here and exercised by the Linux binary test;
\* LocalProcessBoundary owns its timeout/exit mechanics. No business lease is
\* acquired by this action, and readiness alone cannot satisfy CurrentBinding.
ProbeReady == /\ ~deploymentReady /\ deploymentReady' = TRUE
  /\ ~providerSource
  /\ UNCHANGED <<allVars, bindingVars>>
MemoryWaitChanges == /\ providerSource /\ boundSlot # 0 /\ ~effectHeld /\ effects < 2
  /\ \/ (memoryWait = 0 /\ memoryWait' = 1)
     \/ (memoryWait = 1 /\ memoryWait' = 2)
     \/ (memoryWait # 0 /\ memoryWait' = 0)
  /\ UNCHANGED <<allVars, bindingVars, deploymentReady, memoryDrains, providerWait>>
ProviderWaitChanges == /\ providerSource /\ boundSlot # 0 /\ ~effectHeld /\ effects < 2
  /\ memoryWait = 0
  /\ \/ (providerWait = 0 /\ CurrentBinding /\ providerWait' = 1)
     \/ (providerWait = 1 /\ providerWait' = 2)
     \/ (providerWait # 0 /\ providerWait' = 0)
  /\ UNCHANGED <<allVars, bindingVars, deploymentReady, memoryWait, memoryDrains>>
BindingNext == (((((Select \/ WeakSpawn \/ Bind
  \/ CancelEffect \/ AuthorityChanges \/ EnvironmentChanges) /\ UNCHANGED memoryVars)
  \/ CheckEffect \/ CommitEffect) /\ UNCHANGED deploymentReady)
  \/ (ProbeReady /\ UNCHANGED memoryVars) \/ MemoryWaitChanges \/ ProviderWaitChanges)
  /\ UNCHANGED <<registeredWorker, providerSource, providerPhase>>
BindingSpec == BindingInit /\ [][BindingNext]_bindingAll
BindingTypeOK == RootTypeOK /\ selection \in {"unset", "weak", "scoped"}
  /\ deploymentReady \in BOOLEAN /\ registeredWorker \in BOOLEAN
  /\ providerSource \in BOOLEAN /\ providerPhase \in {"preparation", "disposal", "reconstruction", "restore-disposal", "checkpoint"}
  /\ boundSlot \in 0..2 /\ boundGeneration \in 0..2 /\ boundEnvironment \in 0..2
  /\ environmentIdentity \in 1..2 /\ environmentPhase \in {"candidate", "resident", "retiring"}
  /\ sessionExpired \in BOOLEAN /\ effectHeld \in BOOLEAN /\ effectChecked \in BOOLEAN
  /\ badEffect \in BOOLEAN /\ weakStarted \in BOOLEAN /\ effects \in 0..2
  /\ effectPhases \subseteq {"candidate", "resident", "retiring", "provider", "memory", "memory-replay", "preparation", "disposal", "disposal-replay", "reconstruction", "reconstruction-replay", "restore-disposal", "restore-disposal-replay", "checkpoint", "checkpoint-replay"}
  /\ memoryWait \in 0..2 /\ memoryDrains \in 0..2
  /\ providerWait \in 0..2
MemoryCopyDrainedOnce == memoryDrains <= 1
OnlyCurrentBindingCommits == ~badEffect
(* Independent positive-admission oracle: exact fresh provider authority with
   no outstanding wait must remain usable. A second Provider UTC gate cannot
   reject it. No second clock/state machine is introduced: a mutant fixes the
   unrelated clock in its rejecting class for one provider phase. Executable
   tables cross actual past/future Control values with current proof. This is
   enabledness of the next guarded callback, NOT eventual backend completion
   or a proof of clock conversion, suspension, or arbitrary callback duration. *)
FreshProviderEffectRemainsEnabled ==
  (providerSource /\ ~effectHeld /\ effects < 2
   /\ (~registeredWorker \/ RootCurrent)
   /\ "Live" = projection /\ sessionExpired = FALSE
   /\ slotIdentity = boundSlot /\ lease[1] = boundGeneration
   /\ 1 # memoryWait /\ 1 # providerWait) => ENABLED CheckEffect
OnlyProbedDeploymentSelected == selection = "scoped" => deploymentReady
WeakExecutionCannotUpgrade == weakStarted => selection = "weak"
BindingIsWriteOnce == [][boundSlot # 0 => UNCHANGED <<boundSlot, boundGeneration, boundEnvironment>>]_bindingAll
NoCandidateEffectReached == "candidate" \notin effectPhases
NoResidentEffectReached == "resident" \notin effectPhases
NoLocalEffectReached == registeredWorker \/ effects = 0
NoRegisteredEffectReached == ~registeredWorker \/ effects = 0
NoProviderEffectReached == "provider" \notin effectPhases
NoUnreadyProviderEffectReached == ~providerSource \/ deploymentReady \/ effects = 0
NoMemoryAckReached == "memory" \notin effectPhases
NoMemoryReplayReached == "memory-replay" \notin effectPhases
NoEmptyPreparationReached == memoryDrains # 0 \/ "preparation" \notin effectPhases
NoCachedPreparationReached == memoryDrains # 1 \/ "preparation" \notin effectPhases
NoPhysicalDisposalReached == "disposal" \notin effectPhases
NoCachedDisposalReached == "disposal-replay" \notin effectPhases
NoColdReconstructionReached == "reconstruction" \notin effectPhases
NoColdReconstructionReplayReached == "reconstruction-replay" \notin effectPhases
(* Unpublished target cleanup has the same current-effect owner even when
   observation proves absence. This phase models target-lock/backend response
   admission and its retry, not restoration tuple bytes, filesystem locking,
   remote compare-and-delete, or proof that a cancelled remote commit stops. *)
(* Ordinary checkpoint upload/cached WAL uses the same captured Provider root.
   The one put response corresponds to providerWait; each subsequent WAL effect
   re-enters CheckEffect. Terminal put/delete ordering remains in
   LocalProcessRecovery; exact snapshot/object identity remains executable. *)
NoCheckpointUploadReached == "checkpoint" \notin effectPhases
NoCheckpointReplayReached == "checkpoint-replay" \notin effectPhases
NoRestoredDisposalReached == "restore-disposal" \notin effectPhases
NoRestoredDisposalReplayReached == "restore-disposal-replay" \notin effectPhases
=============================================================================
