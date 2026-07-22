-------------------------- MODULE ResourceDispatch --------------------------
EXTENDS Naturals

CONSTANTS MaxConfigVersion, MaxContentEpoch, MaxExecutions

VARIABLES manifest, pinnedConfig, currentConfig, configVersions, contentEpoch,
          resourceState, workerCapability, scopeMatches, projection, executions,
          lastOutcome, lastCapability, lastScopeMatches, lastResourceState,
          lastPinnedConfig, lastLoadedConfig, lastObservedContent,
          lastExecutionCount

vars == <<manifest, pinnedConfig, currentConfig, configVersions, contentEpoch,
          resourceState, workerCapability, scopeMatches, projection, executions,
          lastOutcome, lastCapability, lastScopeMatches, lastResourceState,
          lastPinnedConfig, lastLoadedConfig, lastObservedContent,
          lastExecutionCount>>

Init == /\ manifest = "none"
        /\ pinnedConfig = 0
        /\ currentConfig = 1
        /\ configVersions = {1}
        /\ contentEpoch = 1
        /\ resourceState = "active"
        /\ workerCapability = TRUE
        /\ scopeMatches = TRUE
        /\ projection = FALSE
        /\ executions = 0
        /\ lastOutcome = "none"
        /\ lastCapability = FALSE
        /\ lastScopeMatches = FALSE
        /\ lastResourceState = "none"
        /\ lastPinnedConfig = 0
        /\ lastLoadedConfig = 0
        /\ lastObservedContent = 0
        /\ lastExecutionCount = 0

\* Session creation is the only resolution point. Later publication changes the
\* current pointer but never rewrites this frozen configuration identity.
FreezeAttachment ==
    /\ manifest = "none"
    /\ manifest' = "attach"
    /\ pinnedConfig' = currentConfig
    /\ UNCHANGED <<currentConfig, configVersions, contentEpoch, resourceState,
                    workerCapability, scopeMatches, projection, executions,
                    lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

FreezeEmpty ==
    /\ manifest = "none"
    /\ manifest' = "empty"
    /\ UNCHANGED <<pinnedConfig, currentConfig, configVersions, contentEpoch,
                    resourceState, workerCapability, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

PublishConfig ==
    /\ currentConfig < MaxConfigVersion
    /\ currentConfig' = currentConfig + 1
    /\ configVersions' = configVersions \cup {currentConfig + 1}
    /\ UNCHANGED <<manifest, pinnedConfig, contentEpoch, resourceState,
                    workerCapability, scopeMatches, projection, executions,
                    lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

MutateContent ==
    /\ contentEpoch < MaxContentEpoch
    /\ contentEpoch' = contentEpoch + 1
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    resourceState, workerCapability, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

ToggleCapability ==
    /\ workerCapability' = ~workerCapability
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, resourceState, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

ToggleScopeMatch ==
    /\ scopeMatches' = ~scopeMatches
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, resourceState, workerCapability, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

Suspend ==
    /\ resourceState = "active"
    /\ resourceState' = "suspended"
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, workerCapability, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

Resume ==
    /\ resourceState = "suspended"
    /\ resourceState' = "active"
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, workerCapability, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

Archive ==
    /\ resourceState \in {"active", "suspended"}
    /\ resourceState' = "archived"
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, workerCapability, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

Delete ==
    /\ resourceState \in {"active", "suspended", "archived"}
    /\ resourceState' = "deleted"
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, workerCapability, scopeMatches, projection,
                    executions, lastOutcome, lastCapability, lastScopeMatches,
                    lastResourceState, lastPinnedConfig, lastLoadedConfig,
                    lastObservedContent, lastExecutionCount>>

Activate ==
    /\ manifest = "attach"
    /\ workerCapability
    /\ scopeMatches
    /\ resourceState = "active"
    /\ pinnedConfig \in configVersions
    /\ executions < MaxExecutions
    /\ projection' = TRUE
    /\ executions' = executions + 1
    /\ lastOutcome' = "activated"
    /\ lastCapability' = workerCapability
    /\ lastScopeMatches' = scopeMatches
    /\ lastResourceState' = resourceState
    /\ lastPinnedConfig' = pinnedConfig
    /\ lastLoadedConfig' = pinnedConfig
    /\ lastObservedContent' = contentEpoch
    /\ lastExecutionCount' = executions
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, resourceState, workerCapability, scopeMatches>>

DenyAttachment ==
    /\ manifest = "attach"
    /\ ~(workerCapability /\ scopeMatches /\ resourceState = "active"
          /\ pinnedConfig \in configVersions)
    /\ projection' = FALSE
    /\ lastOutcome' = "denied"
    /\ lastCapability' = workerCapability
    /\ lastScopeMatches' = scopeMatches
    /\ lastResourceState' = resourceState
    /\ lastPinnedConfig' = pinnedConfig
    /\ lastLoadedConfig' = 0
    /\ lastObservedContent' = 0
    /\ lastExecutionCount' = executions
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, resourceState, workerCapability, scopeMatches,
                    executions>>

ApplyEmpty ==
    /\ manifest = "empty"
    /\ workerCapability
    /\ scopeMatches
    /\ projection' = FALSE
    /\ lastOutcome' = "cleared"
    /\ lastCapability' = workerCapability
    /\ lastScopeMatches' = scopeMatches
    /\ lastResourceState' = resourceState
    /\ lastPinnedConfig' = 0
    /\ lastLoadedConfig' = 0
    /\ lastObservedContent' = 0
    /\ lastExecutionCount' = executions
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, resourceState, workerCapability, scopeMatches,
                    executions>>

DenyEmpty ==
    /\ manifest = "empty"
    /\ ~(workerCapability /\ scopeMatches)
    /\ projection' = FALSE
    /\ lastOutcome' = "denied"
    /\ lastCapability' = workerCapability
    /\ lastScopeMatches' = scopeMatches
    /\ lastResourceState' = resourceState
    /\ lastPinnedConfig' = 0
    /\ lastLoadedConfig' = 0
    /\ lastObservedContent' = 0
    /\ lastExecutionCount' = executions
    /\ UNCHANGED <<manifest, pinnedConfig, currentConfig, configVersions,
                    contentEpoch, resourceState, workerCapability, scopeMatches,
                    executions>>

Next == FreezeAttachment \/ FreezeEmpty \/ PublishConfig \/ MutateContent \/
        ToggleCapability \/ ToggleScopeMatch \/ Suspend \/ Resume \/ Archive \/
        Delete \/ Activate \/ DenyAttachment \/ ApplyEmpty \/ DenyEmpty

TypeOK == /\ manifest \in {"none", "attach", "empty"}
          /\ pinnedConfig \in 0..MaxConfigVersion
          /\ currentConfig \in 1..MaxConfigVersion
          /\ configVersions \subseteq 1..MaxConfigVersion
          /\ contentEpoch \in 1..MaxContentEpoch
          /\ resourceState \in {"active", "suspended", "archived", "deleted"}
          /\ workerCapability \in BOOLEAN
          /\ scopeMatches \in BOOLEAN
          /\ projection \in BOOLEAN
          /\ executions \in 0..MaxExecutions
          /\ lastOutcome \in {"none", "activated", "denied", "cleared"}
          /\ lastCapability \in BOOLEAN
          /\ lastScopeMatches \in BOOLEAN
          /\ lastResourceState \in {"none", "active", "suspended", "archived", "deleted"}
          /\ lastPinnedConfig \in 0..MaxConfigVersion
          /\ lastLoadedConfig \in 0..MaxConfigVersion
          /\ lastObservedContent \in 0..MaxContentEpoch
          /\ lastExecutionCount \in 0..MaxExecutions

CurrentConfigExists == currentConfig \in configVersions

FrozenAttachmentKeepsExactConfig ==
    manifest = "attach" => pinnedConfig \in configVersions

ActivationUsesCapabilityScopeLiveStateAndExactPin ==
    lastOutcome = "activated" =>
        /\ lastCapability
        /\ lastScopeMatches
        /\ lastResourceState = "active"
        /\ lastPinnedConfig > 0
        /\ lastLoadedConfig = lastPinnedConfig
        /\ lastObservedContent > 0
        /\ executions = lastExecutionCount + 1

DenialNeverExecutesOrLoads ==
    lastOutcome = "denied" =>
        /\ executions = lastExecutionCount
        /\ lastLoadedConfig = 0
        /\ lastObservedContent = 0

ExplicitEmptyClearsWithoutExecution ==
    lastOutcome = "cleared" =>
        /\ ~projection
        /\ executions = lastExecutionCount
        /\ lastLoadedConfig = 0

NoManifestHasNoProjection == manifest = "none" => ~projection

Spec == Init /\ [][Next]_vars
=============================================================================
