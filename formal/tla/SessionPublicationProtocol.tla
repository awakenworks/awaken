---------------------- MODULE SessionPublicationProtocol ----------------------
EXTENDS Naturals, TLC

CONSTANT MaxCleanupFailures

VARIABLES cleanupPhase, cleanupEffectId, lastAttemptEffectId,
          cleanupAttempts, cleanupFailures, resourceReleaseRequested,
          mode, directReceipt, patchArtifact, manifestArtifact,
          checksumArtifact, bundleReceipt, processUp, downloaded,
          shaVerified, baseVerified, conflictFree, applied,
          testsPassed, scansPassed, validationReceipt

cleanupVars == <<cleanupPhase, cleanupEffectId, lastAttemptEffectId,
                 cleanupAttempts, cleanupFailures, resourceReleaseRequested>>
vars == <<cleanupVars, mode, directReceipt, patchArtifact, manifestArtifact,
          checksumArtifact, bundleReceipt, processUp, downloaded,
          shaVerified, baseVerified, conflictFree, applied,
          testsPassed, scansPassed, validationReceipt>>

NoEffect == "none"
PublicationEffect == "session-publication:session-1"

Cleanup == INSTANCE SessionCleanupKernel WITH
    NoEffect <- NoEffect,
    CleanupEffect <- PublicationEffect,
    MaxCleanupFailures <- MaxCleanupFailures,
    kCleanupPhase <- cleanupPhase,
    kCleanupEffectId <- cleanupEffectId,
    kLastAttemptEffectId <- lastAttemptEffectId,
    kCleanupAttempts <- cleanupAttempts,
    kCleanupFailures <- cleanupFailures,
    kResourceReleaseRequested <- resourceReleaseRequested

Init ==
    /\ Cleanup!Init
    /\ mode = "none"
    /\ directReceipt = FALSE
    /\ patchArtifact = FALSE
    /\ manifestArtifact = FALSE
    /\ checksumArtifact = FALSE
    /\ bundleReceipt = FALSE
    /\ processUp = TRUE
    /\ downloaded = FALSE
    /\ shaVerified = FALSE
    /\ baseVerified = FALSE
    /\ conflictFree = FALSE
    /\ applied = FALSE
    /\ testsPassed = FALSE
    /\ scansPassed = FALSE
    /\ validationReceipt = FALSE

SelectMode(selected) ==
    /\ processUp
    /\ mode = "none"
    /\ selected \in {"direct", "patch_bundle"}
    /\ Cleanup!Fence
    /\ mode' = selected
    /\ UNCHANGED <<directReceipt, patchArtifact, manifestArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

Freeze ==
    /\ processUp
    /\ mode # "none"
    /\ Cleanup!Freeze
    /\ UNCHANGED <<mode, directReceipt, patchArtifact, manifestArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

RecordDirectReceipt ==
    /\ processUp
    /\ cleanupPhase = "requested"
    /\ mode = "direct"
    /\ ~directReceipt
    /\ directReceipt' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, patchArtifact, manifestArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

PublishPatch ==
    /\ processUp /\ cleanupPhase = "requested" /\ mode = "patch_bundle"
    /\ ~patchArtifact
    /\ patchArtifact' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, manifestArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

PublishManifest ==
    /\ processUp /\ cleanupPhase = "requested" /\ mode = "patch_bundle"
    /\ ~manifestArtifact
    /\ manifestArtifact' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

PublishChecksum ==
    /\ processUp /\ cleanupPhase = "requested" /\ mode = "patch_bundle"
    /\ ~checksumArtifact
    /\ checksumArtifact' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

CompleteBundle ==
    /\ processUp /\ mode = "patch_bundle"
    /\ patchArtifact /\ manifestArtifact /\ checksumArtifact
    /\ ~bundleReceipt
    /\ bundleReceipt' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

CompleteCleanup ==
    /\ processUp
    /\ Cleanup!Succeed((mode = "direct" /\ directReceipt)
                       \/ (mode = "patch_bundle" /\ bundleReceipt))
    /\ UNCHANGED <<mode, directReceipt, patchArtifact, manifestArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

Download ==
    /\ mode = "patch_bundle" /\ bundleReceipt /\ ~downloaded
    /\ downloaded' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, shaVerified, baseVerified, conflictFree,
                    applied, testsPassed, scansPassed, validationReceipt>>

VerifySha ==
    /\ downloaded /\ ~shaVerified
    /\ shaVerified' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, baseVerified, conflictFree,
                    applied, testsPassed, scansPassed, validationReceipt>>

VerifyBase ==
    /\ downloaded /\ ~baseVerified
    /\ baseVerified' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, shaVerified, conflictFree,
                    applied, testsPassed, scansPassed, validationReceipt>>

CheckConflict ==
    /\ downloaded /\ ~conflictFree
    /\ conflictFree' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, shaVerified, baseVerified,
                    applied, testsPassed, scansPassed, validationReceipt>>

Apply ==
    /\ shaVerified /\ baseVerified /\ conflictFree /\ ~applied
    /\ applied' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, shaVerified, baseVerified,
                    conflictFree, testsPassed, scansPassed, validationReceipt>>

RecordValidation ==
    /\ applied /\ testsPassed /\ scansPassed /\ ~validationReceipt
    /\ validationReceipt' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, shaVerified, baseVerified,
                    conflictFree, applied, testsPassed, scansPassed>>

PassTests ==
    /\ applied /\ ~testsPassed
    /\ testsPassed' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, shaVerified, baseVerified,
                    conflictFree, applied, scansPassed, validationReceipt>>

PassScans ==
    /\ applied /\ ~scansPassed
    /\ scansPassed' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    processUp, downloaded, shaVerified, baseVerified,
                    conflictFree, applied, testsPassed, validationReceipt>>

CleanupFails ==
    /\ processUp /\ Cleanup!Fail
    /\ UNCHANGED <<mode, directReceipt, patchArtifact, manifestArtifact,
                    checksumArtifact, bundleReceipt, processUp, downloaded,
                    shaVerified, baseVerified, conflictFree, applied,
                    testsPassed, scansPassed, validationReceipt>>

Crash ==
    /\ processUp /\ cleanupPhase # "completed"
    /\ processUp' = FALSE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    downloaded, shaVerified, baseVerified, conflictFree,
                    applied, testsPassed, scansPassed, validationReceipt>>

Restart ==
    /\ ~processUp
    /\ processUp' = TRUE
    /\ UNCHANGED <<cleanupVars, mode, directReceipt, patchArtifact,
                    manifestArtifact, checksumArtifact, bundleReceipt,
                    downloaded, shaVerified, baseVerified, conflictFree,
                    applied, testsPassed, scansPassed, validationReceipt>>

Next ==
    \/ \E selected \in {"direct", "patch_bundle"}: SelectMode(selected)
    \/ Freeze \/ RecordDirectReceipt \/ PublishPatch \/ PublishManifest
    \/ PublishChecksum \/ CompleteBundle \/ CompleteCleanup \/ CleanupFails
    \/ Download \/ VerifySha \/ VerifyBase \/ CheckConflict \/ Apply
    \/ PassTests \/ PassScans \/ RecordValidation \/ Crash \/ Restart

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ Cleanup!TypeOK
    /\ mode \in {"none", "direct", "patch_bundle"}
    /\ directReceipt \in BOOLEAN /\ patchArtifact \in BOOLEAN
    /\ manifestArtifact \in BOOLEAN /\ checksumArtifact \in BOOLEAN
    /\ bundleReceipt \in BOOLEAN /\ processUp \in BOOLEAN
    /\ downloaded \in BOOLEAN /\ shaVerified \in BOOLEAN
    /\ baseVerified \in BOOLEAN /\ conflictFree \in BOOLEAN
    /\ applied \in BOOLEAN /\ testsPassed \in BOOLEAN
    /\ scansPassed \in BOOLEAN /\ validationReceipt \in BOOLEAN

PublicationModeIsExclusive ==
    /\ mode = "direct" => ~bundleReceipt
    /\ mode = "patch_bundle" => ~directReceipt
BundleRequiresAllArtifacts ==
    bundleReceipt => patchArtifact /\ manifestArtifact /\ checksumArtifact
CompletedRequiresExactModeReceipt ==
    cleanupPhase = "completed" =>
        (mode = "direct" /\ directReceipt)
        \/ (mode = "patch_bundle" /\ bundleReceipt)
ApplyRequiresEveryLocalCheck == applied => shaVerified /\ baseVerified /\ conflictFree
ValidationRequiresApplyTestAndScan ==
    validationReceipt => applied /\ testsPassed /\ scansPassed

Safety ==
    /\ TypeOK /\ Cleanup!Safety /\ PublicationModeIsExclusive
    /\ BundleRequiresAllArtifacts /\ CompletedRequiresExactModeReceipt
    /\ ApplyRequiresEveryLocalCheck /\ ValidationRequiresApplyTestAndScan
=============================================================================
