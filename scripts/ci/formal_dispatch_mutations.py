"""Dispatch composition mutation and reachability checks at the canonical CI entry."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_terminal_scan_model_mutants(tlc_jar: Path) -> None:
    check_maintenance_scheduling(tlc_jar)
    # Causes: failing prefix fills a page, healthy suffix is beyond that page,
    # periodic work is fair, and at most one restart forgets the cursor.
    # R1 normal => bounded page + suffix progress. R2 remove cursor advance =>
    # precise temporal starvation counterexample (not syntax/timeout failure).
    # R3 healthy suffix AND wrap must be reachable, excluding vacuous progress.
    source = (ROOT / "formal/tla/TerminalReconciliationScan.tla").read_text()
    config = (ROOT / "formal/tla/TerminalReconciliationScan.cfg").read_text()
    anchor = "              /\\ cursor' = row\n"
    if source.count(anchor) != 1:
        raise SystemExit("Terminal scan cursor mutation anchor drifted")
    check_tlc_model_mutations(tlc_jar, "TerminalReconciliationScan", source, config, [
        (source.replace(anchor, "              /\\ cursor' = cursor\n"), config,
         "temporal:HealthySuffixEventuallyObserved"),
        (source, config.replace("INVARIANTS Safety", "INVARIANTS Safety NoHealthySuffixAfterWrap"),
         "NoHealthySuffixAfterWrap"),
    ])
    # R4 exact-full last page => next empty page wraps. R5 one page can hold
    # every row. R6 empty store => zero reads, no invented candidate. These
    # bounded baselines exercise different pagination shapes, not real SQL.
    for boundary in (
        config.replace("MaxRun = 5", "MaxRun = 4"),
        config.replace("Budget = 2", "Budget = 6"),
        config.replace("MaxRun = 5", "MaxRun = 0").replace("Failed = {1, 2}", "Failed = {}"),
    ):
        check_tlc_model_mutations(tlc_jar, "TerminalReconciliationScan", source, boundary, [])
    print("Terminal scan: bounded fair progress checked; cursor mutant killed; suffix/wrap witnessed")


def check_maintenance_scheduling(tlc_jar: Path) -> None:
    # M1 ownership x execution x timer: all eight scheduler partitions must pass.
    # M2 remove work fence => remote mutation; M3 remove only timer fence =>
    # past-deadline spin; M4 drop execution => temporal failure unless cancelled.
    # Witnesses ensure both useful owner maintenance and remote execution exist.
    source = (ROOT / "formal/tla/DispatchMaintenanceScheduling.tla").read_text()
    config = (ROOT / "formal/tla/DispatchMaintenanceScheduling.cfg").read_text()
    name = "DispatchMaintenanceScheduling"
    changes = [
        ("maintained \\/ OwnsMaintenance", "TRUE", "RemoteNeverMaintains"),
        ("TimerReady == OwnsMaintenance /\\ TimerEnabled", "TimerReady == TimerEnabled", "RemoteNeverSpins"),
        ("drained \\/ (RunsExecution /\\ (hint \\/ poll))", "drained", "temporal:ExecutionProgress"),
    ]
    mutations = []
    for anchor, replacement, violation in changes:
        if source.count(anchor) != 1:
            raise SystemExit(f"Maintenance scheduling mutation anchor drifted: {anchor}")
        mutant_config = config.replace("PROPERTIES ShutdownCompletes ExecutionProgress",
            "PROPERTIES ExecutionProgress") if violation.startswith("temporal:") else config.replace(
                "INVARIANTS Safety", f"INVARIANTS {violation} Safety")
        mutations.append((source.replace(anchor, replacement), mutant_config, violation))
    mutations.append((source, config.replace("INVARIANTS Safety", "INVARIANTS Safety NoRemoteDrainWitness"), "NoRemoteDrainWitness"))
    check_tlc_model_mutations(tlc_jar, name, source, config, mutations)
    for owns in (False, True):
        for execution in (False, True):
            for timer in (False, True):
                row = config.replace("OwnsMaintenance = FALSE", f"OwnsMaintenance = {str(owns).upper()}")
                row = row.replace("RunsExecution = TRUE", f"RunsExecution = {str(execution).upper()}")
                row = row.replace("TimerEnabled = TRUE", f"TimerEnabled = {str(timer).upper()}")
                witnesses = [] if not owns else [(source, row.replace("INVARIANTS Safety", "INVARIANTS Safety NoOwnerMaintenanceWitness"), "NoOwnerMaintenanceWitness")]
                check_tlc_model_mutations(tlc_jar, name, source, row, witnesses)
    print("Maintenance scheduling: 8 partitions checked; 3 mutants killed; owner/remote progress witnessed")


def check_retired_attempt_model_mutants(tlc_jar: Path) -> None:
    # ABA decision table: removing monotonic admission or forgetting the ACK
    # watermark must violate the existing composition's temporal identity rule.
    # A reachable retired identity rejects the vacuous always-block alternative.
    source = (ROOT / "formal/tla/SessionRunProtocol.tla").read_text()
    config = (ROOT / "formal/tla/SessionRunProtocolRetirement.cfg").read_text()
    mutations = []
    for anchor, broken, oracle in [
        ("    /\\ epoch > executionEpoch[run]\n", "", "action:ExecutionIdentityNeverReused"),
        ("    /\\ UNCHANGED executionEpoch\n",
         "    /\\ executionEpoch' = [executionEpoch EXCEPT ![run] = 0]\n",
         "action:ExecutionEpochNeverRegresses"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("attempt retirement mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), config, oracle))
    mutations.append((source, config.replace("INVARIANT Safety", "INVARIANT NoRetiredExecutionReached"),
                      "NoRetiredExecutionReached"))
    check_tlc_model_mutations(tlc_jar, "SessionRunProtocol", source, config, mutations,
                             dependencies=("RunIngressKernel", "SessionActivityKernel", "WorkQueueKernel"))
    print("Attempt retirement: 2 ABA mutants killed; retired execution reachable")


def check_resolution_failure_model_mutant(tlc_jar: Path) -> None:
    check_retired_attempt_model_mutants(tlc_jar)
    # R1 remove the conflict execution gate -> exact safety violation. R2 the
    # original model must REACH a settled conflict, guarding against a vacuous
    # green configuration (e.g. no live Work lease can ever be acquired).
    source = (ROOT / "formal/tla/SessionRunProtocol.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/SessionRunProtocolResolution.cfg").read_text(encoding="utf-8")
    anchor = "    /\\ run \\notin ConflictedRuns\n"
    if source.count(anchor) != 1:
        raise SystemExit("Resolution conflict mutation anchor drifted; review the model")
    witness = config.replace("INVARIANTS Safety ConflictedRunsNeverExecute", "INVARIANTS Safety NoConflictedRunHasSettled")
    check_tlc_model_mutations(tlc_jar, "SessionRunProtocol", source, config, [
        (source.replace(anchor, ""), config, "ConflictedRunsNeverExecute"),
        (source, witness, "NoConflictedRunHasSettled"),
    ], dependencies=("RunIngressKernel", "SessionActivityKernel", "WorkQueueKernel"))
    print("Resolution conflict: execution mutant killed; terminal settlement reachability witnessed")
    # Q1 timeout/drop must not clear a slot with unreturned external work.
    # Q2 require reachable external execution, not a vacuous green model.
    # The full CI configuration separately explores both Workers and 3 epochs;
    # this mutation slice uses 1 epoch to isolate the return-ACK boundary.
    physical_config = (ROOT / "formal/tla/SessionRunProtocol.cfg").read_text()
    physical_config = physical_config.replace("MaxEpoch = 3", "MaxEpoch = 1")
    physical_config = physical_config.replace("MaxHeartbeat = 1", "MaxHeartbeat = 0")
    physical_config = physical_config.replace("MaxTime = 2", "MaxTime = 1")
    return_guard = r"    /\ <<run, candidate, epoch>> \notin externalAttempts" + "\n"
    if source.count(return_guard) != 1:
        raise SystemExit("External return mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "SessionRunProtocol", source, physical_config, [
        (source.replace(return_guard, ""), physical_config.replace("    Safety", "    ExternalAttemptRetainsExactSlot\n    Safety"),
         "ExternalAttemptRetainsExactSlot"),
        (source, physical_config.replace("    Safety", "    NoExternalExecution\n    Safety"), "NoExternalExecution"),
    ], dependencies=("RunIngressKernel", "SessionActivityKernel", "WorkQueueKernel"))
    print("Physical return: unsafe ACK mutant killed; external execution reachable")
    # P1 cancel a physically occupied child, then claim its same-Thread peer.
    # Removing the physical peer gate must permit overlap (not a parser error).
    # P2 the unmodified model must reach that cancelled-peer claim window, so a
    # redundant logical claim gate cannot make the exclusion proof vacuous.
    cancellation = (ROOT / "formal/tla/SessionRunProtocolCancellation.cfg").read_text()
    peer_guard = ("    /\\ \\A peer \\in Runs \\ {run}:\n"
                  "         RunThread(peer) = RunThread(run) => peer \\notin executionStarted\n")
    if source.count(peer_guard) != 1:
        raise SystemExit("Thread physical exclusion mutation anchor drifted")
    check_tlc_model_mutations(tlc_jar, "SessionRunProtocol", source, cancellation, [
        (source.replace(peer_guard, ""), cancellation.replace("INVARIANTS Safety",
            "INVARIANTS OnePhysicalAttemptPerThread Safety"), "OnePhysicalAttemptPerThread"),
        (source, cancellation.replace("INVARIANTS Safety",
            "INVARIANTS NoCancelledPeerClaim Safety"), "NoCancelledPeerClaim"),
    ], dependencies=("RunIngressKernel", "SessionActivityKernel", "WorkQueueKernel"))
    print("Thread exclusion: cancelled peer claim reachable; missing physical guard mutant killed")


def check_maintenance_observation_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    """Cross-check the actual scheduler traces, using the canonical TLC runner."""
    import copy
    import json
    import tempfile
    from render_runtime_refinement_traces import render_observations, module_name

    name = "maintenance-scheduler-composition"
    document = json.loads((trace_dir / f"{name}.json").read_text())
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    # Causes: ownership, timer, role, in-flight begin/return, observed Pending,
    # explicit release, successful/error return. Effects: wrong authority,
    # overlap, early stop, missing join witness and outcome laundering reject.
    # All counterfactuals mutate emitted production observations; the positive
    # inserted hint checks that an irrelevant wake does not fabricate a failure.
    labels = ["remote-work", "missing-return", "overlap", "early-stop",
              "missing-pending", "maintenance-drain", "disabled-scan", "hide-error"]
    with tempfile.TemporaryDirectory(prefix="awaken-maintenance-observation-") as temp:
        directory = Path(temp)
        raw, rendered = directory / "raw", directory / "rendered"
        raw.mkdir()
        path = raw / f"{name}.json"
        def render_case(value):
            path.write_text(json.dumps(value))
            render_observations(raw, rendered, ROOT, manifest, False)
            module = module_name(name)
            return ((rendered / f"{module}.tla").read_text(),
                    (rendered / f"{module}.cfg").read_text())
        baseline, config = render_case(document)
        mutants = []
        for label in labels:
            mutant = copy.deepcopy(document)
            steps = mutant["steps"]
            pending = next(step for step in steps if step["pause"] == "gc" and not step["fail"])
            if label == "remote-work":
                step = next(step for step in steps if not step["owns"])
                step["trace"].insert(0, {"kind": "begin", "operation": "gc"})
            elif label == "missing-return":
                index = next(i for i, event in enumerate(pending["trace"]) if event["kind"] == "ok")
                pending["trace"].pop(index)
            elif label == "overlap":
                index = next(i for i, event in enumerate(pending["trace"]) if event["kind"] == "pending")
                pending["trace"].insert(index, {"kind": "begin", "operation": "relay"})
            elif label == "early-stop":
                stop = pending["trace"].pop()
                index = next(i for i, event in enumerate(pending["trace"]) if event["kind"] == "release")
                pending["trace"].insert(index, stop)
            elif label == "missing-pending":
                pending["trace"] = [event for event in pending["trace"] if event["kind"] != "pending"]
            elif label == "maintenance-drain":
                step = next(step for step in steps if step["topology"] == "reconciler")
                step["trace"].insert(0, {"kind": "drain", "operation": "none"})
            elif label == "disabled-scan":
                step = next(step for step in steps if step["owns"] and not step["terminal"] and step["pause"] == "none")
                source = next(step for step in steps if step["owns"] and step["terminal"] and step["pause"] == "none" and step["topology"] == "service")
                step["trace"] = copy.deepcopy(source["trace"])
            else:
                step = next(step for step in steps if step["fail"])
                next(event for event in step["trace"] if event["kind"] == "error")["kind"] = "ok"
            text, _ = render_case(mutant)
            assert text != baseline, label
            mutants.append((text, config, "constant:TraceRefines"))
        positive = copy.deepcopy(document)
        positive["steps"][0]["trace"].insert(0, {"kind": "hint", "operation": "none"})
        text, _ = render_case(positive)
        check_tlc_model_mutations(tlc_jar, module_name(name), baseline, config, mutants,
            dependencies=("DispatchMaintenanceScheduling",))
        check_tlc_model_mutations(tlc_jar, module_name(name), text, config, [],
            dependencies=("DispatchMaintenanceScheduling",))
        # Rule/schema gates are independent of the model's outcome relation.
        for malformed in ("missing-row", "wrong-label", "missing-event-field"):
            mutant = copy.deepcopy(document)
            if malformed == "missing-row": mutant["steps"].pop()
            elif malformed == "wrong-label": mutant["steps"][0]["owns"] = not mutant["steps"][0]["owns"]
            else: del mutant["steps"][0]["trace"][0]["operation"]
            try: render_case(mutant)
            except ValueError: pass
            else: raise AssertionError(f"maintenance schema mutation survived: {malformed}")
    print("Maintenance production traces: 42 rules, 8 semantic mutants + 3 schema mutants killed; extra hint accepted")


def check_managed_projection_model_mutants(tlc_jar: Path) -> None:
    # Closed publication decision rules R1-R4: stale CAS retries; a regressing
    # scalar/child fence rejects; exact source+content is a completed read;
    # otherwise the paired result/checkpoint, actual CAS and broadcast advance.
    # Finite input qualification includes absent/zero child identity, which a
    # fixed total numeric map cannot express. Concurrent reads remain checked
    # separately by the original and topology configurations of this same model.
    source = (ROOT / "formal/tla/ManagedProjectionPublish.tla").read_text()
    config = (ROOT / "formal/tla/ManagedProjectionPublishInputs.cfg").read_text()
    header = config[:config.index("INVARIANTS")]
    start, end = source.index("Publish(writer) =="), source.index("RefreshFailure(writer) ==")
    original = source[start:end]
    cases = []
    for anchor, replacement, invariant in (
        ("IF observedCacheRevision[writer] # cacheRevision", "IF FALSE", "DecisionIsExact"),
        ("ELSE IF ~Dominates(candidateFence[writer], checkpointFence)", "ELSE IF FALSE", "DecisionIsExact"),
        (r"candidateFence[writer] = checkpointFence /\ ~candidateChanged[writer]", "FALSE", "DecisionIsExact"),
        ('lastOutcome\' = "already_current"', 'lastOutcome\' = "source_regression"', "DecisionIsExact"),
        (r"candidateFence[writer] = checkpointFence /\ ~candidateChanged[writer]", "candidateFence[writer] = checkpointFence", "DecisionIsExact"),
        ("checkpointFence' = candidateFence[writer]", "checkpointFence' = checkpointFence", "ResultAndCheckpointArePaired"),
        ("lastBroadcast' = TRUE", "lastBroadcast' = FALSE", "BroadcastIffPublished"),
        ("cacheRevision' = cacheRevision + 1", "cacheRevision' = cacheRevision", "CacheReceiptMatchesVisible"),
    ):
        if original.count(anchor) != 1:
            raise SystemExit(f"Managed publication mutation anchor drifted: {anchor}")
        cases.append((source[:start] + original.replace(anchor, replacement) + source[end:],
                      header + f"INVARIANTS {invariant}\n", invariant))
    # A missing child must not compare equal to a present child at version zero.
    zero_filled = "ZeroFilled(f) == [c \\in Components |-> IF f[c] = Absent THEN 0 ELSE f[c]]\n\n"
    anchor = "Dominates(candidateFence[writer], checkpointFence)"
    assert original.count(anchor) == 1
    zero_mutant = original.replace(anchor, "Dominates(ZeroFilled(candidateFence[writer]), ZeroFilled(checkpointFence))")
    cases.append((source[:start] + zero_filled + zero_mutant + source[end:],
                  header + "INVARIANTS DecisionIsExact\n", "DecisionIsExact"))
    for witness in ("NoApply", "NoRetry", "NoRegressionRejection", "NoCompletedRead",
                    "NoZeroChildAddition", "NoZeroChildRemovalRejection"):
        cases.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "ManagedProjectionPublish", source, config, cases)
    print("Managed projection publication: 9/9 semantic mutants killed; 6/6 decision/topology witnesses reachable")
    check_managed_projection_refresh_failure_mutants(tlc_jar)


def check_managed_projection_refresh_failure_mutants(tlc_jar: Path) -> None:
    # Failure cause/effect table on the existing concurrent publication model:
    # R1 reading error; R2 built/private-reduction error; R3 another publisher
    # advances CURRENT cache while the failing writer retains its observed base.
    # All three must be reachable. R1/R2/R3 preserve current paired contents,
    # actual cache token and no-broadcast; they do not roll back another writer.
    # Qualification slice: 2 writers, 1 source clock in 0..1, cache in 0..2.
    # Original full concurrency/topology CI configurations remain unchanged.
    source = (ROOT / "formal/tla/ManagedProjectionPublish.tla").read_text()
    config = (ROOT / "formal/tla/ManagedProjectionPublish.cfg").read_text()
    for anchor, replacement in (
        ('Writers = {"writer-a", "writer-b"}', 'Writers = {"writer-a", "writer-b"}'),
        ('Components = {"session", "root-thread", "other-source"}', 'Components = {"session"}'),
        ("OptionalComponents = {}", "OptionalComponents = {}"),
        ("MaxSourceVersion = 2", "MaxSourceVersion = 1"),
        ("MaxCacheRevision = 4", "MaxCacheRevision = 2"),
    ):
        if config.count(anchor) != 1:
            raise SystemExit(f"Managed refresh qualification bound drifted: {anchor}")
        config = config.replace(anchor, replacement)
    header = config[:config.index("INVARIANTS")]
    start, end = source.index("RefreshFailure(writer) =="), source.index("\nNext ==")
    original = source[start:end]
    cases = []
    # Mutate the actual failure action, leaving both the competing publisher
    # and independent frame invariants intact. Parser/tool failures never count.
    for anchor, replacement, invariant in (
        ("UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,",
         "cacheRevision' = observedCacheRevision[writer]\n"
         "    /\\ UNCHANGED <<source, resultFence, checkpointFence,",
         "CacheReceiptMatchesVisible"),
        ("UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,",
         "resultFence' = candidateFence[writer]\n"
         "    /\\ checkpointFence' = candidateFence[writer]\n"
         "    /\\ UNCHANGED <<source, cacheRevision,",
         "FailureAndRejectionStutter"),
        ("lastBroadcast' = FALSE", "lastBroadcast' = TRUE", "BroadcastIffPublished"),
    ):
        if original.count(anchor) != 1:
            raise SystemExit(f"Managed refresh failure mutation anchor drifted: {anchor}")
        cases.append((source[:start] + original.replace(anchor, replacement) + source[end:],
                      header + f"INVARIANTS {invariant}\n", invariant))
    for witness in ("NoReadingFailure", "NoBuiltFailure"):
        cases.append((source, header + f"PROPERTIES {witness}\n", f"action:{witness}"))
    witness = "NoFailureAfterCompetingPublish"
    cases.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "ManagedProjectionPublish", source, config, cases)
    print("Managed refresh failure: 3 current-frame mutants killed; reading/built/competing-publish witnesses reached")
