"""Credential model counterexamples, using the one strict TLC mutation oracle."""

from pathlib import Path

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations



def check_acp_projection_model_mutants(tlc_jar: Path) -> None:
    # P1 omit the post-probe equality; P2 accept another CLI at the same revision;
    # P3 omit the consumer gate; P4 cancel without releasing it. Each must fail
    # its exact named invariant. Reachability witnesses require successful use,
    # a real revoke-during-probe denial and a succeeding peer after cancellation.
    source = (ROOT / "formal/tla/AcpProjectedBindingUse.tla").read_text()
    config = (ROOT / "formal/tla/AcpProjectedBindingUse.cfg").read_text()
    header = "SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, replacement, invariant in (
        (r'before[i] = "active" /\ file = before[i]', r'before[i] = "active"', "ExactPostProbeEvidence"),
        (r'file = before[i] THEN', r'file \in {"active", "otherCli"} THEN', "ExactPostProbeEvidence"),
        (r'phase[i] = "idle" /\ gate = 0', r'phase[i] = "idle"', "OneConsumer"),
        (r'![i] = "cancelled"] /\ gate' + "' = 0", r'![i] = "cancelled"] /\ gate' + "' = gate", "CancelledReleasesGate"),
    ):
        if source.count(anchor) != 1:
            raise SystemExit(f"ACP projection mutation anchor drifted: {invariant}")
        mutations.append((source.replace(anchor, replacement), header + f"INVARIANTS {invariant}\n", invariant))
    for witness in ("NoUsefulGrant", "NoRevokedDenial", "NoConsumerAfterCancellation"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "AcpProjectedBindingUse", source, config, mutations)
    print("ACP projected bindings: 4/4 mutants killed; 3/3 positive witnesses reachable")

def check_acp_binding_model_mutants(tlc_jar: Path) -> None:
    # B1 rejected batch publishes its exclusive prefix => rejected-action failure;
    # B2 check outside the publication lock => two stale checks accept conflicting
    # shared revisions; B3 accepted batch omits a key => incomplete publication.
    # Independent positive witnesses require a compatible union, a genuine
    # conflicting race, and an accepted status update. Parser/tool failure is not
    # evidence: the existing runner requires the exact exit and named property.
    source = (ROOT / "formal/tla/AcpLocalBindingBatch.tla").read_text()
    config = (ROOT / "formal/tla/AcpLocalBindingBatch.cfg").read_text()
    header = "SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, replacement, property_name in (
        ("ELSE bindings\n", "ELSE [bindings EXCEPT ![Own(i)] = Requested(i, Own(i))]\n",
         "action:RejectedBatchHasNoEffect"),
        (r'/\ phase[i] = "idle" /\ mutex = 0', r'/\ phase[i] = "idle"',
         "AcceptedBatchComplete"),
        ("IF k \\in BatchKeys(i) THEN Requested(i, k)", "IF k = Own(i) THEN Requested(i, k)",
         "AcceptedBatchComplete"),
    ):
        if source.count(anchor) != 1:
            raise SystemExit(f"ACP binding mutation anchor drifted: {property_name}")
        action = property_name.startswith("action:")
        selected = property_name.removeprefix("action:")
        keyword = "PROPERTIES" if action else "INVARIANTS"
        mutations.append((source.replace(anchor, replacement),
                          header + f"{keyword} {selected}\n", property_name))
    for witness in ("NoCompatibleUnion", "NoConflictingRace", "NoStatusUpdate"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "AcpLocalBindingBatch", source, config, mutations)
    print("ACP binding batches: 3/3 mutants killed; 3/3 positive witnesses reachable")


def check_acp_observation_model_mutants(tlc_jar: Path) -> None:
    # C1 preserve a previous refresh's probe; C2 repeat one host probe for each
    # Workspace; C3 let credential cardinality decide protocol cardinality;
    # C4 lend one active source's login to a disabled reference. All must fail
    # the named independent invariant, not parsing/timeout. Four witnesses guard
    # against inert projections, mandatory-login coupling and unreachable refresh.
    source = (ROOT / "formal/tla/AcpObservationRefresh.tla").read_text()
    config = (ROOT / "formal/tla/AcpObservationRefresh.cfg").read_text()
    header = "SPECIFICATION Spec\n"
    mutations = []
    for anchor, replacement, invariant in (
        ("cacheGeneration' = 0", "cacheGeneration' = cacheGeneration", "CapabilityUsesThisRefresh"),
        ("probes' = 1", "probes' = Cardinality(batchEnabled)", "OneHostProbePerRefresh"),
        ("rows' = IF selected THEN 1 ELSE 0", "rows' = IF selected THEN Cardinality(batchEnabled) ELSE 0",
         "OneCapabilityRowPerSelectedBackend"),
        ("THEN batchEnabled ELSE {}", "THEN References ELSE {}", "CredentialProjectionNeverWidens"),
    ):
        if source.count(anchor) != 1:
            raise SystemExit(f"ACP observation mutation anchor drifted: {invariant}")
        mutations.append((source.replace(anchor, replacement), header + f"INVARIANTS {invariant}\n", invariant))
    for witness in ("NoSharedActiveProjection", "NoProtocolWithoutBindings", "NoUnselectedBackend",
                    "NoSecondRefreshMissingCli"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "AcpObservationRefresh", source, config, mutations)
    print("ACP observation refresh: 4/4 mutants killed; 4/4 positive witnesses reachable")


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
    # A1 replacement retains source/epoch but invalidates an earlier generation;
    # A2 source rotation changes generation+revision together; A3 reclaim retains
    # generation/source but changes epoch. Keeping the earlier Stage observation
    # lets the independent activation observer detect each delayed stale receipt.
    # Rotation changes two coordinates, so its mutant drops both guards rather
    # than miscounting either individually redundant comparison as a kill.
    for replacements in (
        [(r"/\ stagedGeneration = desiredGeneration", r"/\ TRUE")],
        [(r"/\ stagedGeneration = desiredGeneration", r"/\ TRUE"),
         (r"stagedRevision = desiredRevision /\ stagedEpoch = leaseEpoch", "stagedEpoch = leaseEpoch")],
        [(r"stagedRevision = desiredRevision /\ stagedEpoch = leaseEpoch", "stagedRevision = desiredRevision")],
    ):
        mutant = source
        for anchor, replacement in replacements:
            if mutant.count(anchor) != 1:
                raise SystemExit("MCP activation mutation anchor drifted")
            mutant = mutant.replace(anchor, replacement)
        mutations.append((mutant, header + "INVARIANTS ActivationUsesExactStage\n", "ActivationUsesExactStage"))
    for witness in ("NoClientCallReached", "NoGatewayCallReached", "NoReplacementCallReached",
                    "NoHistoricalRevokedCallReached", "NoIndependentReplacementReached",
                    "NoIndependentRotationReached", "NoIndependentReclaimReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "McpCredentialDelivery", source, config, mutations,
                              heap_mb=1024, timeout_seconds=120)
    print("MCP credential delivery: 9/9 mutants killed; 7 useful/history/independent-authority witnesses reachable")


def check_credential_effect_model_mutants(tlc_jar: Path) -> None:
    # C1 each of the twelve opaque coordinates independently mismatches; C2
    # authority is withdrawn; C3 expiry equals now; C4 the process crashes.
    # M1-M15: bypass each materialization condition => an invalid recorded
    # effect. M16-M27: substitute one receipt coordinate => invalid publication.
    # M28-M30: adoption advances revision/generation/epoch AFTER materialization;
    # M31-M33: withdrawal/expiry/crash in that same wait => no publication.
    # The green base exhausts all 2^12 initial mismatch combinations, all three
    # independent one-step authority successors, and clocks 0..2. Mutants use
    # at most one INITIAL mismatch to isolate the causal guard, but retain all
    # subsequent races. Exact exit + named invariant is required for every kill.
    source = (ROOT / "formal/tla/CredentialEffectBoundary.tla").read_text()
    config = (ROOT / "formal/tla/CredentialEffectBoundary.cfg").read_text()
    header = "SPECIFICATION Spec\nCONSTANT MaxMismatches = 1\nCHECK_DEADLOCK FALSE\n"
    axes = ("workspace", "session", "source", "revision", "holder", "trustDomain",
            "target", "usage", "envelope", "payload", "generation", "epoch")
    mutations = []

    def break_guard(start: str, end: str, anchor: str, broken: str, oracle: str) -> None:
        begin = source.index(start)
        block = source[begin:source.index(end, begin)]
        if block.count(anchor) != 1:
            raise SystemExit(f"Credential effect mutation anchor drifted: {oracle}")
        mutations.append((source.replace(block, block.replace(anchor, broken)),
                          header + f"INVARIANTS {oracle}\n", oracle))

    authority_guard = r"\A axis \in Axes: requested[axis] = current[axis]"
    receipt_guard = r"\A axis \in Axes: receipt[axis] = requested[axis]"
    material_bounds = ("MaterializeExact ==", "RejectInexact ==")
    publication_bounds = ("Publish(receipt) ==", "RejectReceipt(receipt) ==")
    for axis in axes:
        break_guard(*material_bounds, authority_guard,
                    authority_guard.replace("Axes:", f'Axes \\ {{"{axis}"}}:'),
                    "MaterializationRequiresExactClaim")
    for anchor, broken in ((r"authorized /\ now < 2", "now < 2"),
                           (r"authorized /\ now < 2", "authorized"),
                           ('phase = "Prepared" /\\ processUp', 'phase = "Prepared"')):
        break_guard(*material_bounds, anchor, broken, "MaterializationRequiresExactClaim")
    for axis in axes:
        break_guard(*publication_bounds, receipt_guard,
                    receipt_guard.replace("Axes:", f'Axes \\ {{"{axis}"}}:'),
                    "PublicationRequiresMaterialization")
    for axis in ("revision", "generation", "epoch"):
        break_guard(*publication_bounds, authority_guard,
                    authority_guard.replace("Axes:", f'Axes \\ {{"{axis}"}}:'),
                    "PublicationRequiresMaterialization")
    for anchor, broken in ((r"authorized /\ now < 2", "now < 2"),
                           (r"authorized /\ now < 2", "authorized"),
                           ('phase = "Materialized" /\\ processUp', 'phase = "Materialized"')):
        break_guard(*publication_bounds, anchor, broken, "PublicationRequiresMaterialization")
    mutant_count = len(mutations)
    # W1 ordinary success; W2/W3 explicit expired/revoked rejection; W4 success
    # at a successor epoch; W5 stale known material rejected; W6 history retained
    # after revocation. These prevent reject-all and retrospective history loss.
    witnesses = ("NoPublicationReached", "NoExpiredRejectionReached", "NoRevokedRejectionReached",
                 "NoSuccessorPublicationReached", "NoStaleMaterialRejectionReached",
                 "NoRetainedRevokedMaterialReached")
    for witness in witnesses:
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "CredentialEffectBoundary", source, config, mutations,
                              heap_mb=1536, timeout_seconds=180)
    print(f"Credential effect boundary: {mutant_count}/{mutant_count} mutants killed; "
          f"{len(witnesses)} positive/rejection/history witnesses reachable")
