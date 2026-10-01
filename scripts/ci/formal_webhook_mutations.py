"""Negative checks for the one Webhook subscription mutation model."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_webhook_subscription_model_mutants(tlc_jar: Path) -> None:
    source = (ROOT / "formal/tla/WebhookSubscription.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/WebhookSubscription.cfg").read_text(encoding="utf-8")
    header = config.split("INVARIANTS\n", 1)[0]
    # Cause/effect decision table: M1 a visible row applies before its new
    # material exists -> VisibleRowHasMaterial fails. M2 a write starts without
    # its durable intent -> UnpublishedMaterialHasIntent fails. M3 a foreign
    # caller begins an owned mutation -> ForeignWorkspaceCannotMutate fails.
    # The shared runner requires a green baseline and each exact counterexample;
    # parser errors, timeouts and unrelated failures cannot count as kills.
    changes = [
        (
            r'/\ (After(kind) = "none" \/ After(kind) \in materials)',
            r'/\ TRUE',
            "VisibleRowHasMaterial",
        ),
        (r"/\ intent' = TRUE", r"/\ intent' = FALSE", "UnpublishedMaterialHasIntent"),
        (
            r'/\ processUp /\ phase = "idle" /\ requestOwner = "owner"',
            r'/\ processUp /\ phase = "idle"',
            "ForeignWorkspaceCannotMutate",
        ),
    ]
    mutations = []
    for anchor, broken, invariant in changes:
        if source.count(anchor) != 1:
            raise SystemExit(f"Webhook subscription mutation anchor drifted: {invariant}")
        mutations.append(
            (source.replace(anchor, broken), header + f"INVARIANTS {invariant}\n", invariant)
        )
    check_tlc_model_mutations(tlc_jar, "WebhookSubscription", source, config, mutations)
    print("Webhook subscription: 3/3 cause-effect mutants killed")
