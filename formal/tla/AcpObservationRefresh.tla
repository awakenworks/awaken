--------------------------- MODULE AcpObservationRefresh ---------------------------
EXTENDS Naturals, FiniteSets

\* One configured/unselected host backend, two Workspace credential references,
\* two serialized refreshes. WorkerObservationCache owns serialization and lease
\* publication elsewhere; this model owns only the provider's within-batch
\* projection and clearing of cross-batch host evidence. Host/login/binding state
\* and probe-reply backend identity may change between steps. There is no promise
\* of instantaneous revocation,
\* probe cancellation, OS correctness or Rust-to-model refinement.
References == {"a", "b"}
VARIABLES selected, enabled, installed, login, replyMatches, generation, phase, batchEnabled,
          cacheGeneration, cacheInstalled, cacheMatches, probes, rows, available,
          capabilityInstalled, capabilityMatches, evidenceGeneration
vars == <<selected, enabled, installed, login, replyMatches, generation, phase, batchEnabled,
          cacheGeneration, cacheInstalled, cacheMatches, probes, rows, available,
          capabilityInstalled, capabilityMatches, evidenceGeneration>>

Init ==
    /\ selected \in BOOLEAN /\ enabled \in SUBSET References
    /\ installed \in BOOLEAN /\ login \in BOOLEAN /\ replyMatches \in BOOLEAN
    /\ generation = 0 /\ phase = "idle" /\ batchEnabled = {}
    /\ cacheGeneration = 0 /\ cacheInstalled = FALSE /\ cacheMatches = FALSE
    /\ probes = 0 /\ rows = 0 /\ available = {}
    /\ capabilityInstalled = FALSE /\ capabilityMatches = FALSE /\ evidenceGeneration = 0

HostChanges ==
    /\ enabled' \in SUBSET References /\ installed' \in BOOLEAN /\ login' \in BOOLEAN
    /\ replyMatches' \in BOOLEAN
    /\ UNCHANGED <<selected, generation, phase, batchEnabled, cacheGeneration,
        cacheInstalled, cacheMatches, probes, rows, available, capabilityInstalled, capabilityMatches, evidenceGeneration>>

Begin ==
    /\ phase \in {"idle", "complete"} /\ generation < 2
    /\ generation' = generation + 1 /\ phase' = "credentials"
    /\ batchEnabled' = enabled /\ cacheGeneration' = 0
    /\ probes' = 0 /\ rows' = 0 /\ available' = {}
    /\ capabilityInstalled' = FALSE /\ capabilityMatches' = FALSE /\ evidenceGeneration' = 0
    /\ UNCHANGED <<selected, enabled, installed, login, replyMatches, cacheInstalled, cacheMatches>>

Credentials ==
    /\ phase = "credentials" /\ phase' = "capabilities"
    /\ IF batchEnabled # {}
        THEN /\ probes' = 1 /\ cacheGeneration' = generation
             /\ cacheInstalled' = (installed /\ replyMatches)
             /\ cacheMatches' = replyMatches
             /\ available' = IF installed /\ login /\ replyMatches THEN batchEnabled ELSE {}
        ELSE /\ UNCHANGED <<probes, cacheGeneration, cacheInstalled, cacheMatches, available>>
    /\ UNCHANGED <<selected, enabled, installed, login, replyMatches, generation, batchEnabled,
        rows, capabilityInstalled, capabilityMatches, evidenceGeneration>>

Capabilities ==
    /\ phase = "capabilities" /\ phase' = "complete"
    /\ rows' = IF selected THEN 1 ELSE 0
    /\ probes' = probes + IF selected /\ cacheGeneration = 0 THEN 1 ELSE 0
    /\ capabilityInstalled' = (selected /\
        (IF cacheGeneration = 0 THEN (installed /\ replyMatches) ELSE cacheInstalled))
    /\ capabilityMatches' = IF cacheGeneration = 0 THEN replyMatches ELSE cacheMatches
    /\ evidenceGeneration' = IF ~selected THEN 0
        ELSE IF cacheGeneration = 0 THEN generation ELSE cacheGeneration
    /\ UNCHANGED <<selected, enabled, installed, login, replyMatches, generation, batchEnabled,
        cacheGeneration, cacheInstalled, cacheMatches, available>>

Next == HostChanges \/ Begin \/ Credentials \/ Capabilities
TypeOK ==
    /\ selected \in BOOLEAN /\ enabled \subseteq References
    /\ installed \in BOOLEAN /\ login \in BOOLEAN /\ replyMatches \in BOOLEAN /\ generation \in 0..2
    /\ phase \in {"idle", "credentials", "capabilities", "complete"}
    /\ batchEnabled \subseteq References /\ cacheGeneration \in 0..2
    /\ cacheInstalled \in BOOLEAN /\ cacheMatches \in BOOLEAN
    /\ probes \in 0..2 /\ rows \in 0..2 /\ available \subseteq References
    /\ capabilityInstalled \in BOOLEAN /\ capabilityMatches \in BOOLEAN /\ evidenceGeneration \in 0..2
OneHostProbePerRefresh == probes <= 1
OneCapabilityRowPerSelectedBackend == phase = "complete" => rows = IF selected THEN 1 ELSE 0
CredentialProjectionNeverWidens == available \subseteq batchEnabled
CapabilityUsesThisRefresh == phase = "complete" /\ selected => evidenceGeneration = generation

CredentialEvidenceIsBackendScoped == available # {} => cacheMatches
CapabilityEvidenceIsBackendScoped == capabilityInstalled => capabilityMatches

NoRejectedForeignCredential == ~(phase = "complete" /\ batchEnabled # {}
    /\ ~cacheMatches /\ available = {})
NoRejectedForeignCachedCapability == ~(phase = "complete" /\ selected
    /\ cacheGeneration = generation /\ ~cacheMatches /\ ~capabilityInstalled)
NoRejectedForeignFallbackCapability == ~(phase = "complete" /\ selected
    /\ batchEnabled = {} /\ installed /\ ~capabilityMatches /\ ~capabilityInstalled)
NoSharedActiveProjection == ~(phase = "complete" /\ available = References /\ probes = 1)
NoProtocolWithoutBindings == ~(phase = "complete" /\ batchEnabled = {} /\ capabilityInstalled)
NoUnselectedBackend == ~(phase = "complete" /\ ~selected /\ rows = 0)
NoSecondRefreshMissingCli == ~(phase = "complete" /\ generation = 2 /\ selected
    /\ batchEnabled = {} /\ ~capabilityInstalled /\ evidenceGeneration = 2)
Spec == Init /\ [][Next]_vars
=====================================================================================
