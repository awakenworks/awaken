"""Config CAS model mutations using the single strict TLC semantic oracle."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_config_cas_model_mutant(tlc_jar: Path) -> None:
    # R2 absent/positive, R4 present/stale, R5 exhaustion: reject without changing
    # generation OR value. Numeric checks alone miss a rejected payload write.
    # The shared oracle requires a green baseline and each exact invariant;
    # parser errors and unavailable TLC are never credited as detection.
    source = (ROOT / "formal/tla/AggregateCAS.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/AggregateCAS.cfg").read_text(encoding="utf-8")
    guard = "expected[w] = generation /\\ generation < MaxGeneration"
    rejected = "ELSE /\\ UNCHANGED <<generation, value>>"
    if source.count(guard) != 1 or source.count(rejected) != 1:
        raise SystemExit("Config CAS mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "AggregateCAS", source, config, [
        (source.replace(guard, "(generation = 0 \\/ expected[w] = generation) /\\ generation < MaxGeneration"),
         config, "AppliedMatchedGeneration"),
        (source.replace(guard, "expected[w] <= generation /\\ generation < MaxGeneration"),
         config, "AppliedMatchedGeneration"),
        (source.replace(guard, "expected[w] = generation"), config, "TypeOK"),
        (source.replace(rejected, "ELSE /\\ UNCHANGED generation\n"
                        "               /\\ value' = IF generation > 0 THEN w ELSE value"),
         config, "RejectedWritePreservesValue"),
        (source.replace(rejected, "ELSE /\\ generation' = IF generation < MaxGeneration THEN generation + 1 ELSE generation\n"
                        "               /\\ value' = IF generation = 0 THEN w ELSE value"),
         config, "LastWriteIsAtomicCAS"),
    ])
    print("Config CAS mutation score: 5/5 killed (absent, stale, exhaustion, rejected value and actual generation frame)")
