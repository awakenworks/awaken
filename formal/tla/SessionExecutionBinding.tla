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
(* providerSource selects the existing Container renewal callback: it captures
   the same slot/generation before preparation, but no Environment exists yet.
   A callback result is only a durable-fence projection, NOT asynchronous
   provider mutation authority. The check/commit here spans that projection
   under the Worker/slot locks. Backend expiry, clock conversion, operation-id
   bytes and actual I/O after return remain executable/adapter obligations. *)
VARIABLES deploymentReady, registeredWorker, providerSource
VARIABLES selection, boundSlot, boundGeneration, boundEnvironment,
          environmentIdentity, environmentPhase, sessionExpired,
          effectHeld, effectChecked, badEffect, effects, effectPhases, weakStarted
bindingVars == <<selection, boundSlot, boundGeneration, boundEnvironment,
    environmentIdentity, environmentPhase, sessionExpired, effectHeld,
    effectChecked, badEffect, effects, effectPhases, weakStarted>>
bindingAll == <<allVars, bindingVars, deploymentReady, registeredWorker, providerSource>>

BindingInit == RootInit
  /\ registeredWorker \in BOOLEAN
  /\ providerSource \in BOOLEAN
  /\ deploymentReady = FALSE
  /\ selection = "unset" /\ boundSlot = 0 /\ boundGeneration = 0 /\ boundEnvironment = 0
  /\ environmentIdentity = 1 /\ environmentPhase = "candidate" /\ sessionExpired = FALSE
  /\ effectHeld = FALSE /\ effectChecked = FALSE /\ badEffect = FALSE
  /\ effects = 0 /\ effectPhases = {} /\ weakStarted = FALSE

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
  /\ effectHeld' = TRUE /\ effectChecked' = TRUE
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, badEffect, effects, effectPhases, weakStarted>>
CommitEffect == /\ effectHeld /\ effectChecked /\ effects < 2
  \* Independent receipt oracle: changing an admission guard must not also
  \* weaken the assertion against which that mutant is checked.
  /\ badEffect' = (badEffect \/ (registeredWorker /\ ~RootCurrent)
       \/ projection # "Live" \/ sessionExpired
       \/ boundSlot # slotIdentity \/ boundGeneration # lease[1]
       \/ (~providerSource /\ (selection # "scoped"
           \/ boundEnvironment # environmentIdentity \/ environmentPhase = "retiring")))
  /\ effects' = effects + 1
  /\ effectPhases' = effectPhases \cup {IF providerSource THEN "provider" ELSE environmentPhase}
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
BindingNext == (((Select \/ WeakSpawn \/ Bind \/ CheckEffect \/ CommitEffect
  \/ CancelEffect \/ AuthorityChanges \/ EnvironmentChanges)
  /\ UNCHANGED deploymentReady) \/ ProbeReady)
  /\ UNCHANGED <<registeredWorker, providerSource>>
BindingSpec == BindingInit /\ [][BindingNext]_bindingAll
BindingTypeOK == RootTypeOK /\ selection \in {"unset", "weak", "scoped"}
  /\ deploymentReady \in BOOLEAN /\ registeredWorker \in BOOLEAN
  /\ providerSource \in BOOLEAN
  /\ boundSlot \in 0..2 /\ boundGeneration \in 0..2 /\ boundEnvironment \in 0..2
  /\ environmentIdentity \in 1..2 /\ environmentPhase \in {"candidate", "resident", "retiring"}
  /\ sessionExpired \in BOOLEAN /\ effectHeld \in BOOLEAN /\ effectChecked \in BOOLEAN
  /\ badEffect \in BOOLEAN /\ weakStarted \in BOOLEAN /\ effects \in 0..2
  /\ effectPhases \subseteq {"candidate", "resident", "retiring", "provider"}
OnlyCurrentBindingCommits == ~badEffect
OnlyProbedDeploymentSelected == selection = "scoped" => deploymentReady
WeakExecutionCannotUpgrade == weakStarted => selection = "weak"
BindingIsWriteOnce == [][boundSlot # 0 => UNCHANGED <<boundSlot, boundGeneration, boundEnvironment>>]_bindingAll
NoCandidateEffectReached == "candidate" \notin effectPhases
NoResidentEffectReached == "resident" \notin effectPhases
NoLocalEffectReached == registeredWorker \/ effects = 0
NoRegisteredEffectReached == ~registeredWorker \/ effects = 0
NoProviderEffectReached == "provider" \notin effectPhases
NoUnreadyProviderEffectReached == ~providerSource \/ deploymentReady \/ effects = 0
=============================================================================
