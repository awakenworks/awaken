"""Evaluation-evidence model mutations; the shared TLC runner owns the oracle."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_eval_evidence_model_mutants(tlc_jar: Path) -> None:
    source = (ROOT / "formal/tla/EvalEvidence.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/EvalEvidence.cfg").read_text(encoding="utf-8")
    header = config.split("INVARIANTS\n", 1)[0]
    # Cause/effect mutation table: E1 an observation mutates the frozen version;
    # E2 a missing observation can become schema-valid; E3 publication skips an
    # unclassified case. Each mutant must violate its exact invariant after a
    # green baseline; parser failures, timeouts, and unrelated failures earn no
    # credit. W1-W3 prove missing, duplicate, and published schedules reachable.
    changes = [
        (
            "/\\ UNCHANGED <<datasetVersion, scored, schemaValid, published>>",
            "/\\ datasetVersion' = datasetVersion + 1\n"
            "    /\\ UNCHANGED <<scored, schemaValid, published>>",
            "GroundTruthIsFrozen",
        ),
        (
            "schemaValid' = [schemaValid EXCEPT ![case] = observations[case] = 1]",
            "schemaValid' = [schemaValid EXCEPT ![case] = TRUE]",
            "OnlyExactlyOneObservationCanBeValid",
        ),
        (
            "/\\ \\A case \\in Cases : scored[case]",
            "/\\ \\E case \\in Cases : scored[case]",
            "PublishedReportIsComplete",
        ),
    ]
    mutations = []
    for anchor, broken, invariant in changes:
        if source.count(anchor) != 1:
            raise SystemExit(f"Eval evidence mutation anchor drifted: {invariant}")
        mutations.append(
            (source.replace(anchor, broken), header + f"INVARIANTS {invariant}\n", invariant)
        )
    for witness in ("NoMissingCaseScored", "NoDuplicateCaseScored", "NoPublishedReport"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "EvalEvidence", source, config, mutations)
    print("Eval evidence: 3/3 mutants killed; missing, duplicate, and publication schedules reachable")
