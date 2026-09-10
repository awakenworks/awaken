"""BackgroundTask model mutation family owned by the shared mutation CLI."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_background_completion_model_mutant(tlc_jar: Path) -> None:
    # Cause/effect table: C1 remove only the uncertainty guard from canonical
    # settlement; C2/C3 remove the epoch guard from heartbeat/finish. E1 exact
    # unknown retention fails; E2/E3 the stale safety property fails. Each rule
    # uses the shared green-base and exact semantic oracle; infrastructure
    # failure is never a killed mutant.
    source = (ROOT / "formal/tla/BackgroundTask.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/BackgroundTask.cfg").read_text(encoding="utf-8")
    uncertainty_guard = 'p = "Cancelling" /\\ reason # "Indeterminate"'
    heartbeat_guard = """Heartbeat(worker, observedEpoch) ==
    /\\ phase \\in LivePhases
    /\\ worker = owner
    /\\ observedEpoch = epoch
"""
    finish_guard = """Finish(worker, observedEpoch, reason) ==
    /\\ phase \\in LivePhases
    /\\ worker = owner
    /\\ observedEpoch = epoch
"""
    if source.count(uncertainty_guard) != 1:
        raise SystemExit("Background completion mutation anchor drifted; review the model")
    for guard, label in ((heartbeat_guard, "heartbeat"), (finish_guard, "finish")):
        if source.count(guard) != 1:
            raise SystemExit(f"Background {label} fence mutation anchor drifted; review the model")
    stale_oracle = "StaleFenceCannotFinishOrHeartbeat"
    check_tlc_model_mutations(
        tlc_jar,
        "BackgroundTask",
        source,
        config,
        [
            (
                source.replace(uncertainty_guard, 'p = "Cancelling"'),
                config,
                "CompletionProjectionRetainsUnknown",
            ),
            (
                source.replace(
                    heartbeat_guard,
                    heartbeat_guard.replace("    /\\ observedEpoch = epoch\n", ""),
                ),
                config,
                stale_oracle,
            ),
            (
                source.replace(
                    finish_guard,
                    finish_guard.replace("    /\\ observedEpoch = epoch\n", ""),
                ),
                config,
                stale_oracle,
            ),
        ],
    )
    print("Background task mutation score: 3/3 killed (unknown retention, stale heartbeat/finish)")
