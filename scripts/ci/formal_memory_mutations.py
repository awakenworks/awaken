"""Memory physical-owner model mutations; shared TLC runner remains authoritative."""

from pathlib import Path
from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def check_memory_invalidation_model_mutants(tlc_jar: Path) -> None:
    # MI1 missing stop; MI2 return before exit; MI3 detached rather than joined;
    # MI4 stop a peer; MI5 cleanup cannot finish. The real baseline must pass,
    # and exact invariants/liveness must fail (never parser/timeout credit).
    source = (ROOT / "formal/tla/MemoryInvalidationLifecycle.tla").read_text()
    config = (ROOT / "formal/tla/MemoryInvalidationLifecycle.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, oracle in [
        ("stop' = [stop EXCEPT ![i] = TRUE]", "stop' = stop", "StoppingRequestsStop"),
        ('/\\ task[i] = "Exited"\n    /\\ phase\'', '/\\ TRUE\n    /\\ phase\'', "ReturnRequiresJoin"),
        ("joined' = [joined EXCEPT ![i] = TRUE]", "joined' = joined", "ReturnRequiresJoin"),
        ("stop' = [stop EXCEPT ![i] = TRUE]", "stop' = [j \\in Owners |-> TRUE]", "NoPeerStop"),
        ('Finish(i) ==\n    /\\ phase[i] = "Stopping"',
         'Finish(i) ==\n    /\\ FALSE\n    /\\ phase[i] = "Stopping"', "temporal:CleanupEventuallyReturns"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Memory invalidation mutation anchor drifted: {oracle}")
        kind = "PROPERTIES" if oracle.startswith("temporal:") else "INVARIANTS"
        mutations.append((source.replace(anchor, broken),
                          header + f"{kind} {oracle.removeprefix('temporal:')}\n", oracle))
    for witness in ("NoFailedSetupReturned", "NoSuccessfulMountReturned",
                    "NoPeerMountsActive", "NoEarlyExitReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "MemoryInvalidationLifecycle", source, config, mutations)
    print("Memory invalidation mutation score: 5/5 killed; 4 lifecycle schedules reachable")


def check_memory_participant_model_mutants(tlc_jar: Path) -> None:
    # MP1-MP12: cancel/response/pruning/unknown/recovery-failure must retain
    # ownership; resume/reuse must not remount; mutex/current/root/Copy-baseline/
    # settled-ack predicates must hold independently. Exact action/invariant
    # oracles require a green base. Six negative reachability invariants prevent
    # vacuous cancellation, failure, unknown, late-return, Copy-ack and terminal
    # cleanup coverage.
    source = (ROOT / "formal/tla/MemoryParticipantOwnership.tla").read_text()
    config = (ROOT / "formal/tla/MemoryParticipantOwnership.cfg").read_text()
    header = "SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\n"
    changes = [
        ('!.cancelled = TRUE]', '!.cancelled = TRUE, !.held = FALSE]', "RetainedWhilePhysical"),
        ('!.phase = "live", !.stage = "idle"]',
         '!.phase = "live", !.stage = "idle", !.held = FALSE]', "RetainedWhilePhysical"),
        ('s.phase \\in {"empty", "released"}', 's.phase \\in {"empty", "pending", "released"}',
         "RetainedWhilePhysical"),
        ('!.stage = "mount"]', '!.stage = "mount", !.calls = @ + 1]', "SingleExternalMount"),
        ('Reuse == /\\ s.borrower', 'Reuse == /\\ s.borrower', "SingleExternalMount"),
        ('Borrow(c) == /\\ s.borrower = 0', 'Borrow(c) == /\\ TRUE', "action:BorrowIsExclusive"),
        ('/\\ Admitted /\\ CanRetire', '/\\ s.current /\\ CanRetire', "action:EffectsRequireAdmission"),
        ('Retire == /\\ s.stage = "finish" /\\ Admitted',
         'Retire == /\\ s.stage = "finish"', "action:RetirementRequiresAdmission"),
        ('/\\ Admitted /\\ CanRetire', '/\\ Admitted', "action:CopyReplayKeepsBaseline"),
        ('           /\\ s.phase = "live" /\\ s.confirmed /\\ s.mode = "copy"',
         '           /\\ s.phase \\in {"pending", "live"} /\\ s.mode = "copy"',
         "action:AcknowledgementRequiresSettledMount"),
        ('!.phase = "unknown", !.stage = "idle"]',
         '!.phase = "unknown", !.stage = "idle", !.held = FALSE]',
         "RetainedWhilePhysical"),
        ('FailUnknownRecovery == /\\ s.stage = "recover"\n'
         '                       /\\ s\' = [s EXCEPT !.stage = "idle", !.failed = TRUE]',
         'FailUnknownRecovery == /\\ s.stage = "recover"\n'
         '                       /\\ s\' = [s EXCEPT !.stage = "idle", !.failed = TRUE, !.held = FALSE]',
         "RetainedWhilePhysical"),
    ]
    mutations = []
    for anchor, broken, oracle in changes:
        if source.count(anchor) != 1:
            raise SystemExit(f"Memory participant mutation anchor drifted: {oracle}: {anchor}")
        if anchor == broken:
            # Only mutate the Reuse action, not the identical borrower update
            # in other actions. Exact-match reuse must not start a second mount.
            start, end = source.index(anchor), source.index("\nCanRetire ==")
            action = source[start:end]
            mutated = source.replace(action, action.replace('!.borrower = 0]', '!.borrower = 0, !.calls = @ + 1]'))
        else:
            mutated = source.replace(anchor, broken)
        kind = "PROPERTIES" if oracle.startswith("action:") else "INVARIANTS"
        mutations.append((mutated, header + f"{kind} {oracle.removeprefix('action:')}\n", oracle))
    for witness in ("NoCancelledPending", "NoFailedRetained", "NoUnknownRetained",
                    "NoLateMountReturn", "NoCopyAck", "NoCleanedTerminal"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "MemoryParticipantOwnership", source, config, mutations)
    print("Memory participant ownership: 12/12 mutants killed; 6 lifecycle witnesses reachable")


def check_memory_extraction_model_mutants(tlc_jar: Path) -> None:
    """Reject retry/identity mutations at the durable extraction boundary."""
    source = (ROOT / "formal/tla/MemoryExtraction.tla").read_text()
    config = (ROOT / "formal/tla/MemoryExtraction.cfg").read_text()
    header = "SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\nCONSTANTS MaxAttempts = 2 StableRun = stable_run\n"
    def action(name: str, successor: str) -> str:
        start = source.index(f"{name} ==")
        end = source.index(f"\n{successor} ==", start)
        return source[start:end]

    unknown = action("ReturnIndeterminate", "ReturnPermanent")
    retryable = action("ReturnRetryable", "ReturnIndeterminate")
    claim = action("Claim", "StartExtraction")
    recovery = action("RecoverExtraction", "ReturnMutations")
    mutations = [
        (
            source.replace(
                unknown,
                unknown.replace(
                    "/\\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,\n                    runIdentity>>",
                    "/\\ mutations' = TRUE\n    /\\ UNCHANGED <<attempts, runStarts, observations, receipt,\n                    runIdentity>>",
                ),
            ),
            header + "INVARIANTS UnknownHasNoFabricatedWork\n",
            "UnknownHasNoFabricatedWork",
        ),
        (
            source.replace(
                claim,
                claim.replace(
                    '{"pending", "extracted", "stored"}',
                    '{"pending", "extracted", "stored", "execution_indeterminate"}',
                ),
            ),
            header + "INVARIANTS TerminalHasNoClaim\n",
            "TerminalHasNoClaim",
        ),
        (
            source.replace(
                unknown,
                unknown.replace("terminalRunStarts' = runStarts", "terminalRunStarts' = 0"),
            ),
            header + "INVARIANTS UnknownCannotInvokeAgain\n",
            "UnknownCannotInvokeAgain",
        ),
        (
            source.replace(
                retryable,
                retryable.replace(
                    "/\\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,",
                    "/\\ runIdentity' = \"replacement\"\n    /\\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,",
                ).replace(
                    "                    runIdentity, terminalRunStarts>>",
                    "                    terminalRunStarts>>",
                ),
            ),
            header + "INVARIANTS StableAuxiliaryRun\n",
            "StableAuxiliaryRun",
        ),
        (
            source.replace(
                recovery,
                recovery.replace(
                    "/\\ observations' = observations + 1",
                    "/\\ observations' = observations + 1\n    /\\ runStarts' = runStarts + 1",
                ).replace(
                    "/\\ UNCHANGED <<phase, runStarts, mutations, receipt, runIdentity,",
                    "/\\ UNCHANGED <<phase, mutations, receipt, runIdentity,",
                ),
            ),
            header + "INVARIANTS SingleStableRunStart\n",
            "SingleStableRunStart",
        ),
    ]
    if any(mutated == source for mutated, _, _ in mutations):
        raise SystemExit("Memory extraction mutation anchor drifted")
    for witness in ("NoIndeterminateReached", "NoCompletedReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "MemoryExtraction", source, config, mutations)
    print("Memory extraction: 5/5 mutants killed; 2 terminal schedules reachable")
