"""Qualify actual scheduler and held-mutex observations against their one model."""
from copy import deepcopy
import json
from pathlib import Path
import tempfile

from formal_model_mutation_runner import check_tlc_model_mutations

ROOT = Path(__file__).resolve().parents[2]


def check_realization_capacity_observation_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    from render_runtime_refinement_traces import render_observations, module_name
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    # Cause/effect mutations change measured outputs, preserving raw rule inputs.
    # Each must fail the model relation, not only the renderer's shape gate.
    cases = {
        "realization-capacity-cancellation": [
            ("ok", "return", "available", 0),
            ("ok", "return", "available", 2),
            ("ok", "admit-completion", "available", 1),
            ("ok", "queue-completion", "scheduler", "another-scheduler"),
            ("cancel-queued-request", "cancel-queued-request", "available", 1),
            ("queued-deadline", "queued-deadline", "available", 1),
            ("queued-deadline", "queued-deadline", "active", True),
            ("cancel-held-completion", "final", "available", 7),
        ],
        "realization-capacity-mutex": [
            ("finish-wait", "after", "locked", False),
            ("finish-wait", "after", "identity", "replacement-mutex"),
            ("cancel-joined", "after", "lease", "replacement-lease"),
            ("cancel-joined", "after", "closed", False),
            ("cancel-wait", None, "peak", 9),
            ("cancel-wait", "after", "session", "replacement-session"),
        ],
    }
    for name, changes in cases.items():
        document = json.loads((trace_dir / f"{name}.json").read_text())
        with tempfile.TemporaryDirectory(prefix="awaken-capacity-trace-") as temp:
            raw, rendered = Path(temp) / "raw", Path(temp) / "rendered"
            raw.mkdir()
            path = raw / f"{name}.json"
            path.write_text(json.dumps(document))
            render_observations(raw, rendered, ROOT, manifest, False)
            module = module_name(name)
            baseline = (rendered / f"{module}.tla").read_text()
            config = (rendered / f"{module}.cfg").read_text()
            mutants = []
            for rule, cut, field, value in changes:
                mutant = deepcopy(document)
                target = next(s for s in mutant["steps"] if s["rule"] == rule)["observation"]
                if name.endswith("cancellation"):
                    target = next(s for s in target["trace"] if s["event"] == cut)
                elif cut:
                    target = target[cut][0]
                assert target[field] != value
                target[field] = value
                path.write_text(json.dumps(mutant))
                render_observations(raw, rendered, ROOT, manifest, False)
                mutants.append(((rendered / f"{module}.tla").read_text(), config, "constant:TraceRefines"))
            check_tlc_model_mutations(tlc_jar, module, baseline, config, mutants,
                dependencies=("SessionRealizationMutex",))
    print("Realization capacity: 14 actual-observation mutants qualified")
