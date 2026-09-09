"""Config CAS and activation model mutations using the single strict TLC semantic oracle."""

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


def check_config_activation_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect table: P1 deleting source admission must accept a stale
    # candidate; P2 a rejected durable write must violate its actual frame;
    # R1 deleting durable-publication admission must admit an unpublished
    # revision; R3 weakening strict newer comparison revives an equal tombstone;
    # W1 ignoring the older-withdrawal guard rolls the lifecycle back.
    # W2 clearing exact history on withdrawal violates the independent history
    # frame. Five inverse invariants establish reachable positive/negative
    # witnesses on the ORIGINAL model, not just mutants failing vacuously.
    source = (ROOT / "formal/tla/ConfigActivation.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/ConfigActivation.cfg").read_text(encoding="utf-8")
    replacements = (
        ("IF preparedGeneration = authorGeneration", "IF TRUE", "AcceptedPublicationMatchedSource"),
        ("ELSE /\\ UNCHANGED publications", "ELSE /\\ publications' = publications \\cup {preparedGeneration}",
         "RejectedPublicationPreservesDurableSet"),
        ("/\\ revision \\in publications", "/\\ TRUE", "RegisteredHistoryWasDurablyPublished"),
        ("IF revision > installedGeneration", "IF revision >= installedGeneration",
         "RegisteredCurrentRequiresStrictlyNewer"),
        ("IF revision < installedGeneration", "IF FALSE", "ActivationGenerationNeverRollsBack"),
        ("/\\ UNCHANGED <<authorGeneration, preparedGeneration, publications, registeredRevisions>>",
         "/\\ registeredRevisions' = {}\n    /\\ UNCHANGED <<authorGeneration, preparedGeneration, publications>>",
         "ExactHistoryNeverDisappears"),
    )
    mutations = []
    for before, after, invariant in replacements:
        if source.count(before) != 1:
            raise SystemExit(f"Config activation mutation anchor drifted: {before}")
        mutations.append((source.replace(before, after), config, invariant))
    for witness in ("NoAcceptedPublicationWitness", "NoStaleRejectionWitness",
                    "NoDelayedCurrentRegistrationWitness", "NoHistoricalRegistrationWitness",
                    "NoWithdrawalFenceWitness"):
        mutations.append((source, config + f"    {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "ConfigActivation", source, config, mutations)
    print("Config activation: 6/6 mutants killed; 5/5 independent reachability witnesses observed")
