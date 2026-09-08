"""Local process admission, kernel lifetime and recovery mutation family.

The existing CLI dispatch and shared TLC execution/oracle remain authoritative.
Decision tables stay next to their complete semantic mutation groups.
"""

from pathlib import Path
from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_local_process_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect rules LP1 missing atomic spawn policy -> unconfigured handle;
    # LP2 backoff retains mutex -> peer-poll exclusion; LP3 cancelled waiter
    # retains mutex -> abandoned owner; LP4 crash erases descendants -> a false
    # OS guarantee has been smuggled into the model; LP5 a denied group signal
    # succeeds after leader exit -> false cleanup acknowledgement. Each violates
    # its named property; the shared TLC runner rejects parse/tool failures as kills.
    source = (ROOT / "formal/tla/LocalProcessLifecycle.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/LocalProcessLifecycle.cfg").read_text(encoding="utf-8")
    replacements = [
        ("privateGroup' = TRUE /\\ dropPolicy' = TRUE",
         "privateGroup' = FALSE /\\ dropPolicy' = TRUE", "ConfiguredBeforeSpawn"),
        ("/\\ lock' = NoLock\n  /\\ waiter' = [waiter EXCEPT ![w] = IF leaderAlive",
         "/\\ lock' = IF leaderAlive THEN w ELSE NoLock\n  /\\ waiter' = [waiter EXCEPT ![w] = IF leaderAlive",
         "NoParkedWaiterOwnsLock"),
        ("lock' = IF lock = w THEN NoLock ELSE lock", "lock' = lock", "NoParkedWaiterOwnsLock"),
        ("/\\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,\n                  stopRequested, observedExit, signalVars>>",
         "/\\ descendantAlive' = FALSE\n  /\\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive,\n                  stopRequested, observedExit, signalVars>>",
         "action:CrashPreservesOSProcesses"),
        ('signalAccepted\' = (outcome \\in {"accepted", "absent"})',
         'signalAccepted\' = (outcome \\in {"accepted", "absent"} \\/ ~leaderAlive)',
         "SignalDenialIsNotAcknowledged"),
    ]
    mutations = []
    for original, replacement, property_name in replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local process mutation anchor drifted: {property_name}")
        mutations.append((source.replace(original, replacement), config, property_name))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, config, mutations)
    print("Local process mutation score: 5/5 killed (spawn, backoff, cancellation, crash, denied signal)")
    # CG: close cannot pass an admitted launch; a leader-only observation or
    # empty observation without an accepted stop cannot publish quiescence.
    scope_config = (ROOT / "formal/tla/LocalProcessScope.cfg").read_text(encoding="utf-8")
    scope_replacements = [
        # B27 revoke after admission/before GO: removing the final authority
        # check must violate the grant receipt, not an unrelated type property.
        ("/\\ ~authorityHeld /\\ authorityLive\n  /\\ authorityHeld' = TRUE",
         "/\\ ~authorityHeld\n  /\\ authorityHeld' = TRUE", "ScopeGrantWasAuthorized"),
        # G5 a separate thread revokes between check and GO if it does not
        # contend on the SAME guard. No-await alone cannot kill this mutant.
        ("/\\ ~authorityHeld /\\ authorityLive /\\ authorityLive' = FALSE",
         "/\\ authorityLive /\\ authorityLive' = FALSE", "ScopeGrantWasAuthorized"),
        ("/\\ ~scopePermit /\\ scopeClosed' = TRUE", "/\\ scopeClosed' = TRUE",
         "ScopeCloseExcludesLaunch"),
        ("/\\ scopeClosed /\\ scopeKillAccepted /\\ ~leaderAlive /\\ ~descendantAlive",
         "/\\ scopeClosed /\\ scopeKillAccepted /\\ ~leaderAlive", "ScopeReceiptRequiresEmpty"),
        ("/\\ scopeClosed /\\ scopeKillAccepted /\\ ~leaderAlive /\\ ~descendantAlive",
         "/\\ scopeClosed /\\ ~leaderAlive /\\ ~descendantAlive", "ScopeReceiptRequiresAcceptedStop"),
        # B14 execute before join/GO; B15 release admission before ACK; B16
        # cancellation fabricates GO. Each must fail the named handshake rule,
        # not a parser/tool error or an unrelated invariant.
        ("/\\ leaderAlive /\\ goIssued /\\ joined /\\ ~workloadStarted",
         "/\\ leaderAlive /\\ ~workloadStarted", "ScopeWorkloadRequiresGrant"),
        ('/\\ phase = "new" /\\ scopePermit /\\ scopePermit\' = FALSE',
         "/\\ scopePermit /\\ scopePermit' = FALSE", "ScopePendingJoinExcludesClose"),
        ('/\\ launchStage\' = "cancelled"\n  /\\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt,\n                  joined, goIssued, workloadStarted>>',
         '/\\ launchStage\' = "cancelled" /\\ goIssued\' = TRUE\n  /\\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt,\n                  joined, workloadStarted>>',
         "ScopeCancelNeverGrants"),
    ]
    scope_mutations = []
    for original, replacement, property_name in scope_replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local scope mutation anchor drifted: {property_name}")
        scope_mutations.append((source.replace(original, replacement), scope_config, property_name))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, scope_config, scope_mutations)
    print("Local scope mutation score: 8/8 killed (final authority, revoke/grant exclusion, launch/close, descendants, accepted stop, early exec, early release, cancelled GO)")
    # NS decision table: NS1 workload exits before namespace init => no receipt;
    # NS2 init teardown leaves a detached member => not quiescent; NS3 exec after
    # helper/control/deadline loss => reject. NS4 each failure link must eventually
    # begin teardown, given kernel progress. Mutants remove one cause at a time.
    # Safety/action failures require the exact name. For liveness TLC supplies
    # no name, so each mutant CFG selects ONLY the one named temporal property.
    boundary_config = (ROOT / "formal/tla/LocalProcessBoundary.cfg").read_text(encoding="utf-8")
    boundary_replacements = [
        ('((~scopeReceipt /\\ scopeReceipt\') => initState \\in {"absent", "exited"})',
         "TRUE", "BoundaryReceiptObservesInit"),
        ("/\\ leaderAlive' = FALSE /\\ descendantAlive' = FALSE\n  /\\ helperAlive' = FALSE",
         "/\\ leaderAlive' = FALSE /\\ UNCHANGED descendantAlive\n  /\\ helperAlive' = FALSE",
         "BoundaryExitedHasNoMembers"),
        ("((~workloadStarted /\\ workloadStarted') => BoundaryRunnable)",
         "TRUE", "action:BoundaryExecRequiresLiveCutoff"),
    ]
    boundary_mutations = []
    for original, replacement, property_name in boundary_replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local boundary mutation anchor drifted: {property_name}")
        boundary_mutations.append((source.replace(original, replacement), boundary_config, property_name))
    temporal_config = boundary_config.split("PROPERTIES\n", 1)[0]
    temporal_config += "PROPERTIES BoundaryFailureEventuallyTerminates\n"
    for cause in ["phase = \"crashed\"", "~helperAlive", "~controlLive", "~deadlineLive"]:
        original = ('initState = "live" /\\ (phase = "crashed" \\/ ~helperAlive \\/ ~controlLive\n'
                    '    \\/ ~deadlineLive \\/ scopeKillAccepted \\/ launchStage = "cancelled"\n'
                    '    \\/ (workloadStarted /\\ ~leaderAlive))')
        # Preserve the property antecedent; weaken only the transition guard,
        # otherwise the obligation would disappear along with the implementation.
        replacement = original.replace(cause, "FALSE")
        guard = '/\\ BoundaryStopRequired /\\ initState\' = "stopping"'
        if source.count(guard) != 1:
            raise SystemExit("Local boundary stop mutation anchor drifted")
        mutant = source.replace(guard, f'/\\ ({replacement}) /\\ initState\' = "stopping"')
        boundary_mutations.append((mutant, temporal_config, "temporal:BoundaryFailureEventuallyTerminates"))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, boundary_config,
                              boundary_mutations)
    print("Local boundary mutation score: 7/7 killed (init receipt, detached member, expired exec, owner/helper/control/deadline links)")
    # R1 no held current authority at write => unauthorized frame; R2 stale
    # frame overwrites cutoff => shortening; R3 late frame bypasses expiry =>
    # resurrection; R4 receive-time arithmetic => minted transit time; R5 a
    # stalled prefix suppresses clock progress => lost cutoff. Isolate each
    # named property so unrelated type/invariant failures cannot count as kills.
    renewal_config = (ROOT / "formal/tla/LocalProcessRenewal.cfg").read_text(encoding="utf-8")
    renewal_header = renewal_config.split("INVARIANTS\n", 1)[0]
    renewal_replacements = [
        ('/\\ workloadStarted /\\ authorityHeld /\\ controlLive',
         '/\\ workloadStarted /\\ controlLive',
         "RenewalWritesRequireCurrentAuthority", False),
        ("IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) ELSE cutoff",
         "Head(cutoffQueue)", "RenewalCutoffNeverDecreases", True),
        ('/\\ initState = "live" /\\ helperAlive /\\ controlLive /\\ deadlineLive\n  /\\ cutoffQueue',
         '/\\ initState = "live" /\\ helperAlive /\\ controlLive\n  /\\ cutoffQueue',
         "RenewalCannotResurrect", True),
        ("IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) ELSE cutoff",
         "IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) + cutoffClock ELSE cutoff",
         "RenewalCannotMintTransitTime", True),
        ("/\\ cutoffClock < 4 /\\ cutoffClock' = cutoffClock + 1",
         "/\\ ~cutoffPartial /\\ cutoffClock < 4 /\\ cutoffClock' = cutoffClock + 1",
         "temporal:RenewalPartialCannotStallClock", False),
    ]
    renewal_mutations = []
    for original, replacement, property_name, action in renewal_replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local renewal mutation anchor drifted: {property_name}")
        keyword = "PROPERTIES" if action or property_name.startswith("temporal:") else "INVARIANTS"
        selected = property_name.removeprefix("temporal:")
        selected_config = renewal_header + f"{keyword} {selected}\n"
        renewal_mutations.append((source.replace(original, replacement), selected_config,
                                  "action:" + property_name if action else property_name))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, renewal_config,
                              renewal_mutations)
    print("Local renewal mutation score: 5/5 killed (current grant, stale cutoff, resurrection, transit time, partial stall)")
    check_local_recovery_model_mutants(tlc_jar)
    # Refresh classification reuses the exact kernel model. F1 swallowing all
    # errors must fail safety; F2 the old unconditional error must fail benign
    # exit handling; F3 leader-only observation must fail exact-tree safety.
    # Positive witnesses additionally exclude an inert/unreachable classifier.
    refresh_source = (ROOT / "formal/tla/LocalProcessRefresh.tla").read_text(encoding="utf-8")
    refresh_config = (ROOT / "formal/tla/LocalProcessRefresh.cfg").read_text(encoding="utf-8")
    refresh_header = refresh_config.split("INVARIANTS\n", 1)[0]
    refresh_mutations = []
    for original, replacement, property_name in [
        ('IF observation = "exited" THEN "ignored" ELSE "error"',
         '"ignored"', "IgnoredRefreshRequiresExactExit"),
        ('IF observation = "exited" THEN "ignored" ELSE "error"',
         '"error"', "ConfirmedExitSkipsRefreshFailure"),
        ('ELSE IF initState = "exited" THEN "exited" ELSE "live"',
         'ELSE IF ~leaderAlive THEN "exited" ELSE "live"', "IgnoredRefreshRequiresExactExit"),
    ]:
        if refresh_source.count(original) != 1:
            raise SystemExit(f"Local refresh mutation anchor drifted: {property_name}")
        refresh_mutations.append((refresh_source.replace(original, replacement),
                                  refresh_header + f"INVARIANTS {property_name}\n", property_name))
    for witness in ("NoRefreshSkipReached", "NoRefreshErrorReached"):
        refresh_mutations.append((refresh_source, refresh_header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "LocalProcessRefresh", refresh_source, refresh_config,
                              refresh_mutations, dependencies=("LocalProcessLifecycle",))
    print("Local refresh mutation score: 3/3 killed; both skip/error outcomes reachable")
    # Repository R1-R3 compose the same kernel. Test response-only acceptance,
    # leader-only wait, malformed response, failed exit, cancelled caller and
    # Worker crash separately; witnesses require useful and adverse schedules.
    source = (ROOT / "formal/tla/LocalRepositoryEffect.tla").read_text()
    config = (ROOT / "formal/tla/LocalRepositoryEffect.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for replacements, oracle, temporal in [
        ([('/\\ repositoryReply = "valid" /\\ repositoryStatus = "success"\n  /\\ initState = "exited"',
           '/\\ repositoryReply = "valid"')], "RepositoryReturnRequiresExit", False),
        ([('workloadStarted /\\ initState = "exited" /\\ repositoryStatus = "none"',
           'workloadStarted /\\ ~leaderAlive /\\ repositoryStatus = "none"'),
          ('\n  /\\ initState = "exited"\n  /\\ repositoryCaller\'',
           '\n  /\\ ~leaderAlive\n  /\\ repositoryCaller\'')],
         "RepositoryReturnRequiresExit", False),
        ([('repositoryReply = "valid" /\\ repositoryStatus = "success"',
           'repositoryReply # "none" /\\ repositoryStatus = "success"')],
         "RepositoryReturnRequiresValidReply", False),
        ([('repositoryReply = "valid" /\\ repositoryStatus = "success"',
           'repositoryReply = "valid" /\\ repositoryStatus # "none"')],
         "RepositoryReturnRequiresSuccess", False),
        ([('repositoryCaller = "waiting" /\\ phase = "owned"',
           'repositoryCaller # "returned" /\\ phase = "owned"')],
         "RepositoryCancellationIsTerminal", True),
        ([('repositoryCaller = "waiting" /\\ phase = "owned"',
           'repositoryCaller = "waiting"')],
         "RepositoryCrashCannotReturn", True),
    ]:
        mutant = source
        for anchor, broken in replacements:
            if mutant.count(anchor) != 1:
                raise SystemExit(f"Repository effect mutation anchor drifted: {oracle}")
            mutant = mutant.replace(anchor, broken)
        keyword = "PROPERTIES" if temporal else "INVARIANTS"
        mutations.append((mutant, header + f"{keyword} {oracle}\n", "action:" + oracle if temporal else oracle))
    for witness in ("NoRepositoryReturnReached", "NoEarlyRepositoryReplyReached",
                    "NoCancelledRepositoryExitReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "LocalRepositoryEffect", source, config, mutations,
                              dependencies=("LocalProcessLifecycle",))
    print("Local Repository effect mutation score: 6/6 killed; 3 schedules reachable")
    # D9-D14: sampling the OS anchor AFTER the authority wait must mint time;
    # removing expiry rejection must invoke a stale effect. Each oracle is exact.
    source = (ROOT / "formal/tla/LocalProcessDeadline.tla").read_text()
    config = (ROOT / "formal/tla/LocalProcessDeadline.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ("candidate == budgetAnchor + budgetRemaining",
         "candidate == cutoffClock + budgetRemaining", "ConversionNeverMintsAuthorityWaitTime"),
        ('IF candidate > cutoffClock THEN "accepted" ELSE "rejected"',
         '"accepted"', "ExpiredConversionHasNoEffect"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Local deadline mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoAcceptedConversionReached", "NoExpiredConversionReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "LocalProcessDeadline", source, config, mutations,
                              dependencies=("LocalProcessLifecycle",))
    print("Local deadline mutation score: 2/2 killed; accepted/expired conversion reachable")


def check_local_recovery_model_mutants(tlc_jar: Path) -> None:
    # One authority for the former recovery mutants and the async participant
    # extension. REC1-6 retain GO/OS/Hand checks. D1 cancellation loses the file
    # guard; D2 steals another borrow; D3-5/D9 skip fresh checks; D6-7 admit
    # false control/unmount completion; D8 records Removed with a live root;
    # D10 skips post-wait quiescence; D11 skips only the NEXT mount admission;
    # D12 forgets the complete guard set after an expired final teardown.
    # Exact oracles and a green base are required.
    source = (ROOT / "formal/tla/LocalProcessRecovery.tla").read_text()
    config = (ROOT / "formal/tla/LocalProcessRecovery.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for original, replacement, oracle, action in [
        # F1/F2: resource recovery cannot borrow Removing ownership or bypass
        # the exclusive Ready operation. Real SIGKILL/FUSE tests own physical
        # mount/data evidence; these mutants own the abstract admission edges.
        ('ApplyRecovery ==\n  /\\ markerLock = "resource" /\\ RecoveryQuiescent',
         'ApplyRecovery ==\n  /\\ RecoveryQuiescent', "ResourceRecoveryRequiresReadyOwnership", True),
        ('/\\ (operation = "remove" \\/ markerPhase = "ready")',
         '/\\ TRUE', "ResourceRecoveryRequiresReadyOwnership", True),
        ('=> markerRecorded /\\ markerLock = "launch"', '=> markerLock = "launch"', "GoRequiresDurableParticipant", False),
        ('ELSE IF ~observerKnown THEN "unknown"', 'ELSE IF ~observerKnown THEN "empty"', "EffectsRequireKnownQuiescence", True),
        ('ELSE IF initState \\in {"absent", "exited"} THEN "empty" ELSE "live"', 'ELSE IF ~leaderAlive THEN "empty" ELSE "live"', "EffectsRequireKnownQuiescence", True),
        ('recoveryObservation = "empty"\n  /\\ handQuiescent', 'recoveryObservation = "empty"', "EffectsRequireHandQuiescence", True),
        ('/\\ ~sessionReceipt\n  /\\ handQuiescent', '/\\ ~sessionReceipt', "SessionReceiptRequiresAllParticipants", True),
        ('/\\ (~markerRecorded \\/ (observerKnown /\\ initState \\in {"absent", "exited"}))\n  /\\ sessionReceipt\' = TRUE', '/\\ sessionReceipt\' = TRUE', "SessionReceiptRequiresAllParticipants", True),
        ('CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "idle"\n  /\\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars,',
         'CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "idle"\n  /\\ markerLock\' = "free"\n  /\\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded, observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion, handQuiescent, sessionReceipt,',
         "CleanupCancellationRetainsParticipant", True),
        ('EnterCleanup(c, operation) ==\n  /\\ markerLock = "remove" /\\ cleanupCaller = NoLock',
         'EnterCleanup(c, operation) ==\n  /\\ markerLock = "remove"', "CleanupBorrowIsExclusive", True),
        ('/\\ RecoveryQuiescent /\\ cleanupFenceLive', '/\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ (cleanupStage = "memory-next" \\/ cleanupFenceLive) /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "memory-finish"\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage = "memory-finish"\n  /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "physical"\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage = "physical"\n  /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('controlQuiescent\' = TRUE /\\ cleanupStage\' = "memory-admit"', 'controlQuiescent\' = FALSE /\\ cleanupStage\' = "memory-admit"', "RootRemovalFollowsParticipants", False),
        ('mountsReleased\' = TRUE /\\ cleanupStage\' = "physical"', 'mountsReleased\' = FALSE /\\ cleanupStage\' = "physical"', "RootRemovalFollowsParticipants", False),
        ('/\\ cleanupFenceLive /\\ ~rootPresent', '/\\ cleanupFenceLive', "RemovedIsPhysical", False),
        ('/\\ cleanupFenceLive /\\ ~rootPresent', '/\\ ~rootPresent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "physical"\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage = "physical"\n  /\\ cleanupFenceLive', "EffectsRequireKnownQuiescence", True),
        # CP1-3 mutate only the production-corresponding next-effect edges.
        # Store responses themselves remain possible after authority loss.
        ('CheckpointStep("checkpoint", "checkpoint-put", TRUE)',
         'CheckpointStep("checkpoint", "checkpoint-put", FALSE)', "CleanupEffectRequiresFreshFence", True),
        ('CheckpointStep("checkpoint-receipt", "checkpoint-delete-ready", TRUE)',
         'CheckpointStep("checkpoint-receipt", "checkpoint-delete-ready", FALSE)', "CleanupEffectRequiresFreshFence", True),
        ('CheckpointStep("checkpoint-delete-ready", "checkpoint-delete", TRUE)',
         'CheckpointStep("checkpoint-delete-ready", "checkpoint-delete", FALSE)', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "checkpoint-finish"\n  /\\ cleanupFenceLive',
         'cleanupStage = "checkpoint-finish"', "CheckpointSuccessRequiresCurrent", True),
        ('CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "idle"',
         'CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "checkpoint-complete"',
         "CheckpointSuccessRequiresCurrent", True),
    ]:
        if source.count(original) != 1:
            raise SystemExit(f"Local recovery mutation anchor drifted: {oracle}")
        keyword = "PROPERTIES" if action else "INVARIANTS"
        mutations.append((source.replace(original, replacement), header + f"{keyword} {oracle}\n",
                          "action:" + oracle if action else oracle))
    for witness in ("NoCancelledCleanupReached", "NoQueuedCheckpointReached",
                    "NoUnrecordedRemovalReached", "NoLostRemovedReceiptReached",
                    "NoInterMountAdmissionReached", "NoCheckpointSuccessReached",
                    "NoExpiredCheckpointPutResponseReached",
                    "NoExpiredCheckpointDeleteResponseReached",
                    "NoCrashUnknownResourceAttemptReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    # The positive crash witness must observe the effect AFTER owner death;
    # a state with crashed && effect could instead have performed it before.
    mutations.append((source, header + "PROPERTIES NoCrashResourceRecoveryReached\n",
                      "action:NoCrashResourceRecoveryReached"))
    check_tlc_model_mutations(tlc_jar, "LocalProcessRecovery", source, config, mutations,
                              dependencies=("LocalProcessLifecycle",),
                              heap_mb=1024, timeout_seconds=240)
    print("Local recovery mutation score: 25/25 killed; 10 crash/resource/cleanup/checkpoint schedules reachable")
