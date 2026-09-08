"""Two execution objects across one Session replacement; reuse the TLC oracle."""

from pathlib import Path
from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_execution_replacement_model_mutants(tlc_jar: Path) -> None:
    # SR1-SR4 match the real Host projection/retirement tests. Semantic kills
    # distinguish old-wrapper reuse, unknown/live predecessor, cancellation
    # loss and over-denial; parser failures/timeouts never count as evidence.
    source = (ROOT / "formal/tla/SessionExecutionReplacement.tla").read_text()
    config = (ROOT / "formal/tla/SessionExecutionReplacement.cfg").read_text()
    header = "SPECIFICATION ReplacementSpec\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, broken, oracle in [
        ('/\\ observation = "absent"\n  /\\ tree = "absent" /\\ InstallReplacement',
         '/\\ InstallReplacement', "InstallRequiresQuiescence"),
        ("nextCapture' = <<slotIdentity, lease[1], 2>>",
         "nextCapture' = <<boundSlot, boundGeneration, boundEnvironment>>", "FreshCaptureMatchesOwner"),
        ('environmentPhase\' = "retiring"', 'environmentPhase\' = "resident"', "OriginalCannotReopen"),
        ('ExecuteSuccessor == /\\ phase', 'ExecuteSuccessor == /\\ FALSE /\\ phase', "FreshExecutionEnabled"),
        ('phase \\in {"fresh", "ready"} /\\ Successor!CurrentBinding',
         'phase \\in {"fresh", "ready"}', "SuccessorCommitsOnlyCurrent"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"execution replacement mutation anchor drifted: {oracle}")
        mutations.append((source.replace(anchor, broken), header + f"INVARIANTS {oracle}\n", oracle))
    cancel = source[source.index("Cancel =="):source.index("Retire ==")]
    broken = cancel.replace("/\\ cancelled'", '/\\ phase\' = "old"\n  /\\ cancelled\'')
    broken = broken.replace("<<bindingAll, phase, tree", "<<bindingAll, tree")
    mutations.append((source.replace(cancel, broken),
                      header + "PROPERTY CancellationRetainsOwner\n", "action:CancellationRetainsOwner"))
    for witness in ("NoSuccessorReached", "NoCancelledRecoveryReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "SessionExecutionReplacement", source, config, mutations,
                             dependencies=("SessionExecutionBinding", "WorkerExecutionAuthority", "SessionRealizationAuthority"))
    print("Execution replacement: 6/6 mutants killed; successor and two-caller cancelled recovery reachable")
