"""Registration histories refine the existing remote Worker protocol model."""
from pathlib import Path
from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_worker_registration_model_mutants(tlc_jar: Path) -> None:
    # R1 drop seen-incarnation rejection -> A/B/A reopens history;
    # R2 drop complete-history rejection -> imported unknown prefix advances;
    # R3 overwrite rather than append -> older identity authority is forgotten.
    # All must violate RegistrationHistoryIsSingleUse, not parse/tool failures.
    # W1 three fresh incarnations and W2 legacy import remain reachable: an
    # always-reject mutant cannot qualify as a repair. The same Remote protocol
    # model owns this projection; it is not a second Registry state machine.
    source = (ROOT / "formal/tla/RemoteWorkerProtocol.tla").read_text()
    config = (ROOT / "formal/tla/RemoteWorkerProtocolRegistration.cfg").read_text()
    mutations = []
    for anchor, replacement in [
        (r"    /\ physicalIncarnation \notin HistoryValues(w)", "    /\\ TRUE"),
        (r"    /\ 0 \notin HistoryValues(w)", "    /\\ TRUE"),
        ("Append(@, physicalIncarnation)", "<<physicalIncarnation>>"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Worker registration mutation anchor drifted: {anchor}")
        mutations.append((source.replace(anchor, replacement), config.replace(
            "INVARIANTS Safety", "INVARIANTS RegistrationHistoryIsSingleUse Safety"),
            "RegistrationHistoryIsSingleUse"))
    for witness in ["RegistrationWitness", "LegacyWitness"]:
        augmented = source.replace("=============================================================================",
            f"No{witness} == ~{witness}\n=============================================================================")
        mutations.append((augmented, config.replace("INVARIANTS Safety",
            f"INVARIANTS Safety No{witness}"), f"No{witness}"))
    check_tlc_model_mutations(tlc_jar, "RemoteWorkerProtocol", source, config, mutations)
    print("Worker registration: 3/3 semantic mutants killed; fresh replacement and legacy import witnessed")
