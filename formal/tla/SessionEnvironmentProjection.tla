-------------------- MODULE SessionEnvironmentProjection --------------------
EXTENDS TLC

\* Process-local Session Environment projection boundary. Durable effect ids
\* order logical adoption operations; the binding plus immutable generation
\* identify the physical owner. A recovery reader may capture effect A, lose
\* the lifecycle guard, and arrive after effect B has already been published.
CONSTANTS Effects, Generations, Bindings

VARIABLES phase, localEffect, localGeneration, localBinding,
          capturedEffect, capturedGeneration, capturedBinding,
          retirementFence, lastOutcome, lastBeforeEffect, lastAfterEffect,
          lastCapturedGeneration, lastCapturedBinding

VARIABLES retirementCause, lastRecovery
recoveryVars == <<retirementCause, lastRecovery>>

ownerVars == <<phase, localEffect, localGeneration, localBinding,
          capturedEffect, capturedGeneration, capturedBinding,
          retirementFence, lastOutcome, lastBeforeEffect, lastAfterEffect,
          lastCapturedGeneration, lastCapturedBinding, recoveryVars>>

VARIABLE lastSynchronization
vars == <<ownerVars, lastSynchronization>>

NoValue == "none"
RetirementCauses == {"Revocation", "Discard", "Terminal", "Checkpoint"}

Init ==
    /\ lastSynchronization = [observed |-> TRUE, closed |-> FALSE,
                                allowed |-> FALSE, admitted |-> TRUE, after |-> "Resident"]
    /\ phase = "Resident"
    /\ localEffect \in Effects
    /\ localGeneration \in Generations
    /\ localBinding \in Bindings
    /\ capturedEffect = NoValue
    /\ capturedGeneration = NoValue
    /\ capturedBinding = NoValue
    /\ retirementFence = NoValue
    /\ lastOutcome = NoValue
    /\ lastBeforeEffect = NoValue
    /\ lastAfterEffect = NoValue
    /\ lastCapturedGeneration = NoValue
    /\ lastCapturedBinding = NoValue
    /\ retirementCause = NoValue
    /\ lastRecovery = [exact |-> FALSE, closed |-> FALSE, allowed |-> FALSE,
                        recoverable |-> TRUE, cleared |-> FALSE,
                        cause |-> NoValue,
                        before |-> "Resident", after |-> "Resident"]

Capture(effect, generation, binding) ==
    /\ UNCHANGED recoveryVars
    /\ effect \in Effects
    /\ generation \in Generations
    /\ binding \in Bindings
    /\ capturedEffect' = effect
    /\ capturedGeneration' = generation
    /\ capturedBinding' = binding
    /\ UNCHANGED <<phase, localEffect, localGeneration, localBinding,
                    retirementFence, lastOutcome, lastBeforeEffect,
                    lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

\* The canonical local adoption path may publish a newer logical effect for
\* the same physical Environment while a captured recovery read is waiting.
PublishNewEffect(effect) ==
    /\ UNCHANGED recoveryVars
    /\ phase = "Resident"
    /\ effect \in Effects
    /\ effect # localEffect
    /\ localEffect' = effect
    /\ UNCHANGED <<phase, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    retirementFence, lastOutcome, lastBeforeEffect,
                    lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

BeginRetiring(cause) ==
    /\ cause \in RetirementCauses
    /\ retirementCause' = cause
    /\ UNCHANGED lastRecovery
    /\ phase = "Resident"
    /\ phase' = "Retiring"
    /\ retirementFence' = localEffect
    /\ UNCHANGED <<localEffect, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    lastOutcome, lastBeforeEffect, lastAfterEffect,
                    lastCapturedGeneration, lastCapturedBinding>>

\* Installation is observational for Resident/Retiring owners. Exact effects
\* replay; stale effects on the same physical owner are absorbed; a different
\* binding or generation is rejected. None may relabel the local owner.
InstallCaptured ==
    /\ phase # "Vacant"
    /\ UNCHANGED recoveryVars
    /\ capturedEffect \in Effects
    /\ lastBeforeEffect' = localEffect
    /\ lastAfterEffect' = localEffect
    /\ lastCapturedGeneration' = capturedGeneration
    /\ lastCapturedBinding' = capturedBinding
    /\ lastOutcome' =
         IF capturedGeneration # localGeneration \/ capturedBinding # localBinding
         THEN "rejected"
         ELSE IF capturedEffect = localEffect THEN "exact" ELSE "stale"
    /\ capturedEffect' = NoValue
    /\ capturedGeneration' = NoValue
    /\ capturedBinding' = NoValue
    /\ UNCHANGED <<phase, localEffect, localGeneration, localBinding,
                    retirementFence>>

\* Cause/effect rules: exact captured owner + closed physical observation +
\* rebuild policy may retire Resident, Discard or Revocation, never Terminal or
\* Checkpoint. !exact abstracts every cause/identity/binding/Arc ABA mismatch.
\* Provider observation truth and Rust identity comparison remain TCB: this
\* finite transition model does not claim whole implementation refinement.
RecoverClosed(exact, closed, allowed) ==
    /\ phase # "Vacant"
    /\ LET recoverable == phase = "Resident" \/
                            retirementCause \in {"Revocation", "Discard"}
           admit == exact /\ closed /\ allowed /\ recoverable
       IN /\ phase' = IF admit THEN "Vacant" ELSE phase
          /\ lastRecovery' = [exact |-> exact, closed |-> closed,
                               allowed |-> allowed, recoverable |-> recoverable,
                               cause |-> retirementCause,
                               cleared |-> admit, before |-> phase, after |-> phase']
    /\ UNCHANGED <<localEffect, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    retirementFence, retirementCause, lastOutcome,
                    lastBeforeEffect, lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

\* Caller boundary for an already-resident Worker. Physical loss is an
\* independent provider input, never inferred from the local Arc. This action
\* abstracts one observation plus policy decision, NOT atomic provider IO or
\* perpetual liveness after that observation. Reuse the owner transition above.
\* Decision rules S1 Ready => retain; S2 closed+continuity => reject/retain;
\* S3 closed+rebuild => clear the exact owner before replacement publication.
\* The Rust driver test crosses Runtime cached/evicted with S1-S3 and removes
\* the actual Local root; K3d separately checks replacement UID/PVC/file bytes.
SynchronizeResident(closed, allowed) ==
    /\ phase = "Resident"
    /\ LET observed == TRUE
       IN /\ IF observed THEN RecoverClosed(TRUE, closed, allowed)
                           ELSE UNCHANGED ownerVars
          /\ lastSynchronization' = [observed |-> observed, closed |-> closed,
                                       allowed |-> allowed,
                                       admitted |-> ~observed \/ ~closed \/ allowed,
                                       after |-> phase']

OwnerNext ==
    \/ \E effect \in Effects, generation \in Generations, binding \in Bindings:
           Capture(effect, generation, binding)
    \/ \E effect \in Effects: PublishNewEffect(effect)
    \/ \E cause \in RetirementCauses: BeginRetiring(cause)
    \/ InstallCaptured
    \/ \E exact, closed, allowed \in BOOLEAN: RecoverClosed(exact, closed, allowed)

Next ==
    \/ (OwnerNext /\ UNCHANGED lastSynchronization)
    \/ \E closed, allowed \in BOOLEAN: SynchronizeResident(closed, allowed)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"Resident", "Retiring", "Vacant"}
    /\ retirementCause \in RetirementCauses \cup {NoValue}
    /\ localEffect \in Effects
    /\ localGeneration \in Generations
    /\ localBinding \in Bindings
    /\ capturedEffect \in Effects \cup {NoValue}
    /\ capturedGeneration \in Generations \cup {NoValue}
    /\ capturedBinding \in Bindings \cup {NoValue}
    /\ retirementFence \in Effects \cup {NoValue}
    /\ lastOutcome \in {NoValue, "exact", "stale", "rejected"}
    /\ lastBeforeEffect \in Effects \cup {NoValue}
    /\ lastAfterEffect \in Effects \cup {NoValue}
    /\ lastCapturedGeneration \in Generations \cup {NoValue}
    /\ lastCapturedBinding \in Bindings \cup {NoValue}

StaleProjectionNeverRollsBackLocalEffect ==
    lastOutcome = "stale" => lastAfterEffect = lastBeforeEffect

RejectedProjectionNeverRelabelsLocalEffect ==
    lastOutcome = "rejected" => lastAfterEffect = lastBeforeEffect

ExactProjectionIsAlsoAnIdentityStutter ==
    lastOutcome = "exact" => lastAfterEffect = lastBeforeEffect

AbsorptionRequiresExactPhysicalOwner ==
    lastOutcome \in {"exact", "stale"} =>
        lastCapturedGeneration = localGeneration /\
        lastCapturedBinding = localBinding

RetiringKeepsExactEffectFence ==
    phase = "Retiring" => localEffect = retirementFence

ProjectionNeverRewritesTheRetirementFence ==
    lastOutcome \in {"exact", "stale", "rejected"} /\ phase = "Retiring" =>
        localEffect = retirementFence

ClosedRecoveryIsExactAndComplete ==
      /\ lastRecovery.recoverable = (lastRecovery.before = "Resident" \/
                                      lastRecovery.cause \in {"Revocation", "Discard"})
      /\ lastRecovery.cleared = (lastRecovery.exact /\ lastRecovery.closed /\
                                 lastRecovery.allowed /\ lastRecovery.recoverable)
      /\ (lastRecovery.cleared => lastRecovery.after = "Vacant")
      /\ (~lastRecovery.cleared => lastRecovery.after = lastRecovery.before)

ResidentSynchronizationObservesPhysicalOwner ==
    /\ lastSynchronization.observed
    /\ lastSynchronization.admitted =
         (~lastSynchronization.closed \/ lastSynchronization.allowed)
    /\ lastSynchronization.after =
         IF lastSynchronization.closed /\ lastSynchronization.allowed
         THEN "Vacant" ELSE "Resident"

Safety ==
    /\ ResidentSynchronizationObservesPhysicalOwner
    /\ TypeOK
    /\ StaleProjectionNeverRollsBackLocalEffect
    /\ RejectedProjectionNeverRelabelsLocalEffect
    /\ ExactProjectionIsAlsoAnIdentityStutter
    /\ AbsorptionRequiresExactPhysicalOwner
    /\ RetiringKeepsExactEffectFence
    /\ ProjectionNeverRewritesTheRetirementFence
    /\ ClosedRecoveryIsExactAndComplete
=============================================================================
