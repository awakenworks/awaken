"""Bounded successive Session replacements; reuse one model and TLC oracle."""

from pathlib import Path
from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_k8s_process_observation_model_mutants(tlc_jar: Path) -> None:
    # KPT: UID replacement, phase-only failure, omitted participant, and absent
    # historical binding must fail closed. A reachable positive exit excludes
    # an observer that always returns Unknown. No Dispatch protocol is copied.
    source = (ROOT / "formal/tla/K8sProcessObservation.tla").read_text()
    config = (ROOT / "formal/tla/K8sProcessObservation.cfg").read_text()
    mutations = []
    for anchor, broken in [
        ("g = current", "TRUE"),
        ("/\\ CompleteExit(current)", "/\\ (CompleteExit(current) \\/ current \\in failed)"),
        ("/\\ CompleteExit(current)", "/\\ (Domain(current) \\intersect exited # {})"),
        ("/\\ g \\in bound", "/\\ g \\in Generations"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Kubernetes process observation mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), config, "Safety"))
    mutations.append((source, config.replace("INVARIANT Safety", "INVARIANT NoObservedExit"), "NoObservedExit"))
    check_tlc_model_mutations(tlc_jar, "K8sProcessObservation", source, config, mutations)
    print("Kubernetes process observation: 4 unsafe-evidence mutants killed; real-exit witness reached")


def check_execution_replacement_model_mutants(tlc_jar: Path) -> None:
    # SR/SC/UC match the real Host projection/retirement tests. Semantic kills
    # distinguish old-wrapper reuse, unknown/live predecessor, cancellation
    # loss and over-denial; parser failures/timeouts never count as evidence.
    source = (ROOT / "formal/tla/SessionExecutionReplacement.tla").read_text()
    config = (ROOT / "formal/tla/SessionExecutionReplacement.cfg").read_text()
    header = "SPECIFICATION ReplacementSpec\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, broken, oracle in [
        ('/\\ observation = "absent"\n  /\\ tree = "absent" /\\ InstallReplacement',
         '/\\ InstallReplacement', "InstallRequiresQuiescence"),
        ("nextCapture' = <<slotIdentity, lease[1], lease[1]>>",
         "nextCapture' = <<boundSlot, boundGeneration, boundEnvironment>>", "FreshCaptureMatchesOwner"),
        ('environmentPhase\' = "retiring"', 'environmentPhase\' = "resident"', "OriginalCannotReopen"),
        ('ExecuteSuccessor == /\\ phase', 'ExecuteSuccessor == /\\ FALSE /\\ phase', "FreshExecutionEnabled"),
        ('phase \\in {"fresh", "ready"} /\\ Successor!CurrentBinding',
         'phase \\in {"fresh", "ready"}', "SuccessorCommitsOnlyCurrent"),
        ('Retire == /\\ caller', 'Retire == /\\ originalPhase = "resident" /\\ caller',
         "CommittedOwnerRetirementEnabled"),
        ('/\\ observedReceipt = environmentIdentity\n  /\\ phase\' = "ready"',
         '/\\ phase\' = "ready"', "PublicationHasReadback"),
        ('/\\ rootReceipt = environmentIdentity /\\ observedReceipt # environmentIdentity\n',
         '/\\ observedReceipt # environmentIdentity\n', "ReadbackNamesCommittedRoot"),
        ('/\\ rootReceipt # environmentIdentity /\\ rootWrites[environmentIdentity] < 2',
         '/\\ rootWrites[environmentIdentity] < 2', "RootReceiptWrittenOnce"),
        ('CommitRoot == /\\ caller # 0 /\\ phase = "fresh" /\\ Successor!CurrentBinding',
         'CommitRoot == /\\ caller # 0 /\\ phase = "fresh"', "RootCommitIsCurrent"),
        ('CommitRoot == /\\ caller', 'CommitRoot == /\\ FALSE /\\ caller', "FreshRootCommitEnabled"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"execution replacement mutation anchor drifted: {oracle}")
        mutations.append((source.replace(anchor, broken), header + f"INVARIANTS {oracle}\n", oracle))
    cancel = source[source.index("Cancel =="):source.index("Retire ==")]
    broken = cancel.replace("/\\ cancelled'", '/\\ phase\' = "old"\n  /\\ cancelled\'')
    broken = broken.replace("<<bindingAll, phase, tree", "<<bindingAll, tree")
    mutations.append((source.replace(cancel, broken),
                      header + "PROPERTY CancellationRetainsOwner\n", "action:CancellationRetainsOwner"))
    for witness in ("NoSuccessorReached", "NoCancelledRecoveryReached", "NoCandidateSuccessorReached",
                    "NoUncommittedRecoveryReached", "NoLostResponseRecoveryReached", "NoPostCommitCancellationRecoveryReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "SessionExecutionReplacement", source, config, mutations,
                             dependencies=("SessionExecutionBinding", "WorkerExecutionAuthority", "SessionRealizationAuthority"))
    print("Execution replacement: 12/12 mutants killed; 6 recovery witnesses including lost commit response and post-commit cancellation")
    chain_config = (ROOT / "formal/tla/SessionExecutionReplacementChain.cfg").read_text()
    chain_header = chain_config.split("PROPERTIES", 1)[0]
    # MC C/E table: both intermediate adoptions may independently lose the
    # response before or after commit. Every feasible 00/01/10/11 schedule must
    # reach generation 4. Receipt and immutable-capture mutants specifically
    # exercise bugs invisible to a single-successor model. Baseline is checked
    # before any kill; tool failures never stand in for a semantic violation.
    chain_mutations = []
    for anchor, broken, oracle in [
        ('/\\ observedReceipt = environmentIdentity\n',
         '/\\ (observedReceipt = environmentIdentity \\/ (environmentIdentity > 2 /\\ rootReceipt > 0))\n',
         "PublicationHasReadback"),
        ('/\\ observedReceipt\' = 0\n', '/\\ observedReceipt\' = rootReceipt\n',
         "ReadbackIsInvocationSpecific"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"replacement chain mutation anchor drifted: {oracle}")
        chain_mutations.append((source.replace(anchor, broken), chain_header + f"INVARIANTS {oracle}\n", oracle))
    capture = "/\\ captures' = [captures EXCEPT ![lease[1]] = nextCapture']"
    if source.count(capture) != 1:
        raise SystemExit("replacement chain capture anchor drifted")
    chain_mutations.append((source.replace(capture,
        "/\\ captures' = [captures EXCEPT ![lease[1]] = nextCapture', ![2] = nextCapture']"),
        chain_header + "PROPERTY SuccessorCaptureIsWriteOnce\n", "action:SuccessorCaptureIsWriteOnce"))
    # MC-M: rematerializing the source at Adopt must rewind a prior local edit;
    # attributing a successor write to an old wrapper must violate exact writer
    # identity. Keep a positive final Memory write witness to exclude over-denial.
    for anchor, broken, kind, oracle in [
        ("(Adopt /\\ UNCHANGED copyWriter)", "(Adopt /\\ copyWriter' = 0)",
         "PROPERTY", "CopyNeverRewinds"),
        ("ExecuteSuccessor /\\ copyWriter' = environmentIdentity",
         "ExecuteSuccessor /\\ copyWriter' = boundEnvironment",
         "INVARIANTS", "SuccessorMemoryIsCurrent"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"replacement Copy mutation anchor drifted: {oracle}")
        check = f"action:{oracle}" if kind == "PROPERTY" else oracle
        chain_mutations.append((source.replace(anchor, broken), chain_header + f"{kind} {oracle}\n", check))
    for witness in ("NoMultiRecovery00", "NoMultiRecovery01", "NoMultiRecovery10", "NoMultiRecovery11", "NoMultiMemoryRecovery"):
        chain_mutations.append((source, chain_header + f"CONSTRAINT RecoveryWitnessScope\nINVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "SessionExecutionReplacement", source, chain_config, chain_mutations,
                             dependencies=("SessionExecutionBinding", "WorkerExecutionAuthority", "SessionRealizationAuthority"),
                             heap_mb=4096, timeout_seconds=1800, workers=8)
    print("Execution replacement chain: 5/5 mutants killed; 5 successive cancellation/Memory recovery witnesses")
    check_terminal_owner_model_mutants(tlc_jar)


def check_terminal_owner_model_mutants(tlc_jar: Path) -> None:
    # U/HM rows: terminal handoff of the same hidden owner, not a second
    # production lifecycle. Exhaust two callers, cancellation, replacement,
    # participant failure (stutter), durable preparation and lost delete reply.
    source = (ROOT / "formal/tla/SessionEnvironmentProjection.tla").read_text()
    config = (ROOT / "formal/tla/SessionEnvironmentTerminalOwner.cfg").read_text()
    mutations = []
    for anchor, broken, oracle in [
        ('!.phase = "Retiring"]', '!.phase = "Retiring", !.identity = 1]', "TerminalIdentityUnchanged"),
        ('!.retried = TRUE, !.wrapper = 1', '!.retried = TRUE, !.wrapper = 2', "TerminalRetainsExactOwner"),
        ('!.cancelled = @ \\cup {terminalOwner.caller}',
         '!.phase = "Vacant", !.cancelled = @ \\cup {terminalOwner.caller}', "TerminalRetainsExactOwner"),
        ('/\\ TerminalCurrent /\\ exact /\\ terminalOwner.durable',
         '/\\ TerminalCurrent /\\ exact', "TerminalDisposalRequiresParticipants"),
        ('/\\ TerminalCurrent /\\ exact /\\ terminalOwner.durable',
         '/\\ exact /\\ terminalOwner.durable', "TerminalEffectsAreCurrentAndExact"),
        ('/\\ TerminalCurrent /\\ exact /\\ terminalOwner.durable',
         '/\\ TerminalCurrent /\\ terminalOwner.durable', "TerminalEffectsAreCurrentAndExact"),
        ('/\\ terminalOwner.participants = TerminalParticipants', '/\\ TRUE', "TerminalDisposalRequiresParticipants"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"terminal owner mutation anchor drifted: {oracle}")
        mutations.append((source.replace(anchor, broken),
                          config.replace("TerminalOwnerSafety", oracle), oracle))
    for witness in ("NoUncommittedTerminalCompletion", "NoLostResponseTerminalCompletion",
                    "NoCancelledAuxiliaryTakeoverCompletion"):
        mutations.append((source, config.replace("TerminalOwnerSafety", witness), witness))
    check_tlc_model_mutations(tlc_jar, "SessionEnvironmentProjection", source, config, mutations)
    print("Terminal owner: 7/7 semantic mutants killed; 3 terminal completion witnesses")
