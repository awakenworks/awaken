-------------------- MODULE SessionExecutionBinding --------------------
EXTENDS WorkerExecutionAuthority

(* Compose the existing Worker receipt and Session slot kernel, rather than
   invent another lease. One scope, two slot/environment identities and two
   Session generations. Root -> slot locks span the callback; the split
   check/commit exposes a lock-release mutant. Time within a synchronous grant
   is abstracted atomically here; LocalProcessDeadline/Boundary own physical
   cutoff and callback-delay bounds. This is not an OS or Rust refinement proof.
   The separate Session-expiry bit does NOT equate wall time with BOOTTIME. *)
VARIABLES deploymentReady
VARIABLES selection, boundSlot, boundGeneration, boundEnvironment,
          environmentIdentity, environmentPhase, sessionExpired,
          effectHeld, effectChecked, badEffect, effects, effectPhases, weakStarted
bindingVars == <<selection, boundSlot, boundGeneration, boundEnvironment,
    environmentIdentity, environmentPhase, sessionExpired, effectHeld,
    effectChecked, badEffect, effects, effectPhases, weakStarted>>
bindingAll == <<allVars, bindingVars, deploymentReady>>

BindingInit == RootInit
  /\ deploymentReady = FALSE
  /\ selection = "unset" /\ boundSlot = 0 /\ boundGeneration = 0 /\ boundEnvironment = 0
  /\ environmentIdentity = 1 /\ environmentPhase = "candidate" /\ sessionExpired = FALSE
  /\ effectHeld = FALSE /\ effectChecked = FALSE /\ badEffect = FALSE
  /\ effects = 0 /\ effectPhases = {} /\ weakStarted = FALSE

CurrentBinding == /\ RootCurrent /\ projection = "Live" /\ ~sessionExpired
  /\ selection = "scoped" /\ boundSlot = slotIdentity
  /\ boundGeneration = lease[1] /\ boundEnvironment = environmentIdentity
  /\ environmentPhase \in {"candidate", "resident"}

Select == /\ selection = "unset" /\ deploymentReady /\ selection' = "scoped"
  /\ UNCHANGED <<allVars, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, effectHeld,
       effectChecked, badEffect, effects, effectPhases, weakStarted>>
WeakSpawn == /\ selection \in {"unset", "weak"}
  /\ selection' = "weak" /\ weakStarted' = TRUE
  /\ UNCHANGED <<allVars, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, effectHeld,
       effectChecked, badEffect, effects, effectPhases>>
Bind == /\ selection = "scoped" /\ boundSlot = 0 /\ projection = "Live"
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
  /\ badEffect' = (badEffect \/ ~RootCurrent \/ projection # "Live" \/ sessionExpired
       \/ selection # "scoped" \/ boundSlot # slotIdentity \/ boundGeneration # lease[1]
       \/ boundEnvironment # environmentIdentity \/ environmentPhase = "retiring")
  /\ effects' = effects + 1
  /\ effectPhases' = effectPhases \cup {environmentPhase}
  /\ effectHeld' = FALSE /\ effectChecked' = FALSE
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, weakStarted>>
CancelEffect == /\ effectHeld /\ effectHeld' = FALSE /\ effectChecked' = FALSE
  /\ UNCHANGED <<allVars, selection, boundSlot, boundGeneration, boundEnvironment,
       environmentIdentity, environmentPhase, sessionExpired, badEffect, effects, effectPhases, weakStarted>>

(* Existing authority transition owners; no shadow renew/revoke implementation.
   Peer cleanup/fresh Session counts are irrelevant to this one scope and stay
   fixed; their full interleavings remain in WorkerExecutionAuthority. *)
AuthorityChanges == (RequestHeartbeat \/ ApplyHeartbeat \/ CloseRoot \/ Tick
    \/ InstallReplacement \/ RenewProjection
    \/ ((SendRenewal \/ BeginRevoke(1) \/ RecreateClosedSlot
          \/ (RootCurrent /\ RecreateSlot)) /\ UNCHANGED rootVars))
  /\ ~effectHeld /\ UNCHANGED bindingVars
EnvironmentChanges == /\ ~effectHeld
  /\ \/ (/\ environmentPhase = "candidate" /\ environmentPhase' = "resident"
          /\ UNCHANGED <<environmentIdentity, sessionExpired>>)
     \/ (/\ environmentPhase \in {"candidate", "resident"}
          /\ environmentPhase' = "retiring" /\ UNCHANGED <<environmentIdentity, sessionExpired>>)
     \/ (/\ environmentIdentity = 1 /\ environmentIdentity' = 2
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
  /\ UNCHANGED <<allVars, bindingVars>>
BindingNext == ((Select \/ WeakSpawn \/ Bind \/ CheckEffect \/ CommitEffect
  \/ CancelEffect \/ AuthorityChanges \/ EnvironmentChanges)
  /\ UNCHANGED deploymentReady) \/ ProbeReady
BindingSpec == BindingInit /\ [][BindingNext]_bindingAll
BindingTypeOK == RootTypeOK /\ selection \in {"unset", "weak", "scoped"}
  /\ deploymentReady \in BOOLEAN
  /\ boundSlot \in 0..2 /\ boundGeneration \in 0..2 /\ boundEnvironment \in 0..2
  /\ environmentIdentity \in 1..2 /\ environmentPhase \in {"candidate", "resident", "retiring"}
  /\ sessionExpired \in BOOLEAN /\ effectHeld \in BOOLEAN /\ effectChecked \in BOOLEAN
  /\ badEffect \in BOOLEAN /\ weakStarted \in BOOLEAN /\ effects \in 0..2
  /\ effectPhases \subseteq {"candidate", "resident", "retiring"}
OnlyCurrentBindingCommits == ~badEffect
OnlyProbedDeploymentSelected == selection = "scoped" => deploymentReady
WeakExecutionCannotUpgrade == weakStarted => selection = "weak"
BindingIsWriteOnce == [][boundSlot # 0 => UNCHANGED <<boundSlot, boundGeneration, boundEnvironment>>]_bindingAll
NoCandidateEffectReached == "candidate" \notin effectPhases
NoResidentEffectReached == "resident" \notin effectPhases
=============================================================================
