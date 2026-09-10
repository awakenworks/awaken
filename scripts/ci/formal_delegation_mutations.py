"""Delegation model mutations; the shared TLC runner owns the oracle."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_delegation_model_mutants(tlc_jar: Path) -> None:
    source = (ROOT / "formal/tla/Delegation.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/Delegation.cfg").read_text(encoding="utf-8")

    duplicate = (
        'DuplicateRequest(child) ==\n'
        '    /\\ linkStatus[child] # "Absent"\n'
        '    /\\ UNCHANGED vars'
    )
    duplicate_mutant = (
        'DuplicateRequest(child) ==\n'
        '    /\\ linkStatus[child] # "Absent"\n'
        "    /\\ started' = started + 1\n"
        '    /\\ UNCHANGED <<parentEnded, childState, linkStatus, resultState, childOwner,\n'
        '                    childEpoch, cancelDelivered>>'
    )
    start_guard = (
        'StartChild(child, owner) ==\n'
        '    /\\ linkStatus[child] = "Open"'
    )
    start_mutant = (
        'StartChild(child, owner) ==\n'
        '    /\\ linkStatus[child] \\in {"Absent", "Open"}'
    )
    delivery_guard = (
        'DeliverCancellation(child) ==\n'
        '    /\\ parentEnded\n'
        '    /\\ linkStatus[child] = "CancelRequested"'
    )
    delivery_mutant = (
        'DeliverCancellation(child) ==\n'
        '    /\\ parentEnded\n'
        '    /\\ linkStatus[child] # "Absent"'
    )

    anchors = (
        (duplicate, duplicate_mutant, "ConstraintsHold"),
        (start_guard, start_mutant, "FirstClassIdentity"),
        (delivery_guard, delivery_mutant, "CancellationDeliveryFollowsIntent"),
    )
    mutations = []
    for anchor, replacement, invariant in anchors:
        if source.count(anchor) != 1:
            raise SystemExit(f"Delegation mutation anchor drifted: {invariant}")
        mutations.append((source.replace(anchor, replacement), config, invariant))

    # D1 proves that the configured remote relationship can be requested,
    # durably cancelled, and delivered. The intentionally false invariant must
    # therefore fail; an unreachable cancellation action earns no coverage.
    witness = source.replace(
        "\n=============================================================================",
        "\nNoRemoteCancellationDelivered ==\n"
        "    \\A child \\in RemoteChildren: ~cancelDelivered[child]\n"
        "\n=============================================================================",
    )
    witness_config = config.replace(
        "    TypeOK",
        "    NoRemoteCancellationDelivered\n    TypeOK",
        1,
    )
    mutations.append((witness, witness_config, "NoRemoteCancellationDelivered"))

    check_tlc_model_mutations(
        tlc_jar,
        "Delegation",
        source,
        config,
        mutations,
        dependencies=("RuntimeVocabulary",),
    )
    print(
        "Delegation: 3/3 mutants killed; remote cancellation delivery schedule reachable"
    )
