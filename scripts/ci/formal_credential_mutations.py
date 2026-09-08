"""Credential model counterexamples, using the one strict TLC mutation oracle."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_mcp_credential_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect table (the model owns the relation, not this runner):
    # M1 wrong requested generation, M2 wrong source revision, M3 call after
    # process loss => a concrete unauthorized effect observation. Process-up
    # and rebuilt-generation checks jointly exclude crash, so M3 removes both.
    # M4 adopted revocation keeps an accepting route => invalid durable acceptance.
    # M5 activation of a revoked stage => invalid durable acceptance.
    # M6 unknown delivery pair => invalid frozen selection.
    # W1/W2 each supported mode calls; W3 replacement calls; W4 a legal past
    # call remains recorded after revocation. The last case prevents an oracle
    # that retroactively invalidates effects or erases them on revocation.
    # A removed call-time revocation check alone is redundant under the model's
    # atomic Revoke; we do not count that equivalent mutant as independent proof.
    source = (ROOT / "formal/tla/McpCredentialDelivery.tla").read_text()
    config = (ROOT / "formal/tla/McpCredentialDelivery.cfg").read_text()
    header = "SPECIFICATION Spec\nCONSTANT MaxGeneration = 3\n"
    call_start, call_end = source.index("Call(generation, revision) =="), source.index("\nCrash ==")
    call = source[call_start:call_end]
    mutations = []
    for replacements in (
        [(r"generation = acceptingGeneration /\ revision = acceptingRevision", r"revision = acceptingRevision")],
        [(r"generation = acceptingGeneration /\ revision = acceptingRevision", r"generation = acceptingGeneration")],
        [(r"processUp /\ acceptingGeneration > 0", "acceptingGeneration > 0"),
         (r"/\ processGeneration = acceptingGeneration", r"/\ TRUE")],
    ):
        mutant = call
        for anchor, replacement in replacements:
            if mutant.count(anchor) != 1:
                raise SystemExit("MCP call mutation anchor drifted")
            mutant = mutant.replace(anchor, replacement)
        mutations.append((source.replace(call, mutant), header + "INVARIANTS NoUnauthorizedEffect\n", "NoUnauthorizedEffect"))
    for anchor, replacement, invariant in (
        ("IF acceptingRevision = revision", "IF FALSE", "AcceptingRevisionIsLive"),
        (r"/\ stagedRevision \notin revokedRevisions", r"/\ TRUE", "AcceptingRevisionIsLive"),
        ("IF Allowed(mode, holder, mechanism)", "IF TRUE", "SelectedModeIsAlwaysAdmissible"),
    ):
        if source.count(anchor) != 1:
            raise SystemExit(f"MCP delivery mutation anchor drifted: {invariant}")
        mutations.append((source.replace(anchor, replacement), header + f"INVARIANTS {invariant}\n", invariant))
    for witness in ("NoClientCallReached", "NoGatewayCallReached", "NoReplacementCallReached", "NoHistoricalRevokedCallReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "McpCredentialDelivery", source, config, mutations)
    print("MCP credential delivery: 6/6 mutants killed; 4 useful/history witnesses reachable")
