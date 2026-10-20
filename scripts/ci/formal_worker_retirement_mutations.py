"""Bounded Actor/source retirement checks through the canonical TLC oracle."""
from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_retained_worker_retirement_model_mutants(tlc_jar: Path) -> None:
    model = (ROOT / "formal/tla/RetainedWorkerRetirement.tla").read_text()
    config = (ROOT / "formal/tla/RetainedWorkerRetirement.cfg").read_text()
    prepared = (ROOT / "formal/tla/RetainedWorkerRetirementPrepared.cfg").read_text()
    unknown = (ROOT / "formal/tla/RetainedWorkerRetirementUnknown.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    unknown_header = unknown.split("INVARIANTS\n", 1)[0]
    # M1 waiting for cleanup before retiring the old-code Actor recreates
    # the correction cycle. M2 waiting for its continually renewed cleanup
    # lease has the same liveness defect. M3 accepting a successful source
    # prefix loses another source. M4/M5 weaken successor fencing or acknowledge
    # cleanup early. M6/M7 require actual retirement and cleanup reachability.
    # Retained and already-prepared data have separate positive cases; unknown
    # sources get safety only. Tool/parser failure never counts as a kill.
    changes = [
        (r'/\ ~oldStopped /\ ~permit', r'/\ ~oldStopped /\ ~permit /\ cleanupAck',
         "temporal:QualifiedRetirementEventuallyCompletes", header),
        (r'/\ ~oldStopped /\ ~permit', r'/\ ~oldStopped /\ ~permit /\ ~leaseLive',
         "temporal:QualifiedRetirementEventuallyCompletes", header),
        (r"/\ \A s \in Sources : SourceRecoverable(s)",
         r"/\ \E s \in Sources : SourceRecoverable(s)",
         "UnpreparedSourceSurvivesStop", unknown_header),
        (r'ClaimNew == /\ oldExited /\ createClosed /\ ~leaseLive /\ rootOwner = "original"',
         r'ClaimNew == /\ rootOwner = "original"',
         "SuccessorRequiresExitAndCreateClosure", header),
        (r"/\ \A s \in Sources : prepared[s] /\ ~present[s]", r"/\ TRUE",
         "RetirementIsNotCleanupAck", header),
    ]
    mutations = []
    for anchor, broken, invariant, case_header in changes:
        if model.count(anchor) != 1:
            raise SystemExit(f"retained Worker retirement anchor drifted: {invariant}")
        clause = ("PROPERTIES " + invariant.removeprefix("temporal:")) if invariant.startswith("temporal:") else "INVARIANTS " + invariant
        mutations.append((model.replace(anchor, broken), case_header + clause + "\n", invariant))
    for witness in ("NoRetirementReached", "NoCleanupReached"):
        mutations.append((model, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "RetainedWorkerRetirement", model, config, mutations)
    for case in (prepared, unknown):
        check_tlc_model_mutations(tlc_jar, "RetainedWorkerRetirement", model, case, [])
    print("retained Worker retirement: 3 bounded configurations, 7/7 semantic counterexamples")
