"""Erasure checkpoint mutations using the single strict TLC semantic oracle."""

from pathlib import Path

from formal_model_mutation_runner import check_tlc_model_mutations


ROOT = Path(__file__).resolve().parents[2]


def check_erasure_checkpoint_model_mutants(tlc_jar: Path) -> None:
    # Causes/effects: M1 fresh-but-regressive proposal bypasses the subset guard;
    # M2 accepted publication drops a target. Both must violate the existing
    # durable-history invariant, not fail parsing or infrastructure. The negative
    # witness proves M1's exact-revision proposal is reachable in the green model.
    source = (ROOT / "formal/tla/ErasureSaga.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/ErasureSaga.cfg").read_text(encoding="utf-8")
    guard = r"  /\ durableCompleted \subseteq localCompleted[r]"
    publish = r"  /\ durableCompleted' = localCompleted[r]"
    if source.count(guard) != 1 or source.count(publish) != 1:
        raise SystemExit("Erasure checkpoint mutation anchor drifted")
    # Restrict only the reported invariant, keeping the full canonical constants
    # and transition graph. This isolates each intended semantic rejection.
    targeted = config.split("INVARIANTS", 1)[0] + "INVARIANTS CheckpointNeverForgets\n"
    witness = config.split("INVARIANTS", 1)[0] + "INVARIANTS NoFreshRegressiveProposal\n"
    check_tlc_model_mutations(
        tlc_jar,
        "ErasureSaga",
        source,
        config,
        [
            (source.replace(guard, "  /\\ TRUE"), targeted, "CheckpointNeverForgets"),
            (
                source.replace(publish, "  /\\ durableCompleted' = {}"),
                targeted,
                "CheckpointNeverForgets",
            ),
            (source, witness, "NoFreshRegressiveProposal"),
        ],
    )
    print(
        "Erasure checkpoint: 2/2 semantic mutants killed; "
        "fresh regressive proposal reachable"
    )
