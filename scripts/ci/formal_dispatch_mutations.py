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
