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

vars == <<phase, localEffect, localGeneration, localBinding,
          capturedEffect, capturedGeneration, capturedBinding,
          retirementFence, lastOutcome, lastBeforeEffect, lastAfterEffect,
          lastCapturedGeneration, lastCapturedBinding>>

NoValue == "none"

Init ==
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

Capture(effect, generation, binding) ==
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
    /\ phase = "Resident"
    /\ effect \in Effects
    /\ effect # localEffect
    /\ localEffect' = effect
    /\ UNCHANGED <<phase, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    retirementFence, lastOutcome, lastBeforeEffect,
                    lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

BeginRetiring ==
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

Next ==
    \/ \E effect \in Effects, generation \in Generations, binding \in Bindings:
           Capture(effect, generation, binding)
    \/ \E effect \in Effects: PublishNewEffect(effect)
    \/ BeginRetiring
    \/ InstallCaptured

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"Resident", "Retiring"}
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

Safety ==
    /\ TypeOK
    /\ StaleProjectionNeverRollsBackLocalEffect
    /\ RejectedProjectionNeverRelabelsLocalEffect
    /\ ExactProjectionIsAlsoAnIdentityStutter
    /\ AbsorptionRequiresExactPhysicalOwner
    /\ RetiringKeepsExactEffectFence
    /\ ProjectionNeverRewritesTheRetirementFence
=============================================================================
