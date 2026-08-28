------------------------- MODULE SessionCleanupKernel -------------------------
EXTENDS Naturals

CONSTANTS NoEffect, CleanupEffect, MaxCleanupFailures

VARIABLES kCleanupPhase, kCleanupEffectId, kLastAttemptEffectId,
          kCleanupAttempts, kCleanupFailures, kResourceReleaseRequested

kVars == <<kCleanupPhase, kCleanupEffectId, kLastAttemptEffectId,
           kCleanupAttempts, kCleanupFailures, kResourceReleaseRequested>>

Init ==
    /\ kCleanupPhase = "not_requested"
    /\ kCleanupEffectId = NoEffect
    /\ kLastAttemptEffectId = NoEffect
    /\ kCleanupAttempts = 0
    /\ kCleanupFailures = 0
    /\ kResourceReleaseRequested = FALSE

Fence ==
    /\ kCleanupPhase = "not_requested"
    /\ kCleanupPhase' = "fenced"
    /\ kCleanupEffectId' = CleanupEffect
    /\ UNCHANGED <<kLastAttemptEffectId, kCleanupAttempts,
                    kCleanupFailures, kResourceReleaseRequested>>

Freeze ==
    /\ kCleanupPhase = "fenced"
    /\ kCleanupPhase' = "requested"
    /\ kResourceReleaseRequested' = TRUE
    /\ UNCHANGED <<kCleanupEffectId, kLastAttemptEffectId,
                    kCleanupAttempts, kCleanupFailures>>

Fail ==
    /\ kCleanupPhase = "requested"
    /\ kCleanupFailures < MaxCleanupFailures
    /\ kCleanupFailures' = kCleanupFailures + 1
    /\ kCleanupAttempts' = kCleanupAttempts + 1
    /\ kLastAttemptEffectId' = kCleanupEffectId
    /\ UNCHANGED <<kCleanupPhase, kCleanupEffectId,
                    kResourceReleaseRequested>>

Succeed(ready) ==
    /\ kCleanupPhase = "requested"
    /\ ready
    /\ kCleanupPhase' = "completed"
    /\ kCleanupAttempts' = kCleanupAttempts + 1
    /\ kLastAttemptEffectId' = kCleanupEffectId
    /\ UNCHANGED <<kCleanupEffectId, kCleanupFailures,
                    kResourceReleaseRequested>>

TypeOK ==
    /\ kCleanupPhase \in {"not_requested", "fenced", "requested", "completed"}
    /\ kCleanupEffectId \in {NoEffect, CleanupEffect}
    /\ kLastAttemptEffectId \in {NoEffect, CleanupEffect}
    /\ kCleanupAttempts \in 0..(MaxCleanupFailures + 1)
    /\ kCleanupFailures \in 0..MaxCleanupFailures
    /\ kResourceReleaseRequested \in BOOLEAN

StableEffectIdentity ==
    /\ kCleanupPhase # "not_requested" => kCleanupEffectId = CleanupEffect
    /\ kLastAttemptEffectId # NoEffect => kLastAttemptEffectId = kCleanupEffectId

CompletionRequiresFrozenResources ==
    kCleanupPhase = "completed" => kResourceReleaseRequested

Safety ==
    /\ TypeOK
    /\ StableEffectIdentity
    /\ CompletionRequiresFrozenResources
=============================================================================
