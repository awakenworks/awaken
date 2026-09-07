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
    # MP1-MP10: cancel/response/pruning must retain ownership; resume/reuse must
    # not remount; mutex/current/root/Copy-baseline/settled-ack predicates must
    # hold independently. Exact action/invariant oracles require a green base.
    # Five negative reachability invariants prevent vacuous cancellation, failed
    # compensation, late-return, Copy-ack and terminal-cleanup coverage.
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
    for witness in ("NoCancelledPending", "NoFailedRetained", "NoLateMountReturn",
                    "NoCopyAck", "NoCleanedTerminal"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "MemoryParticipantOwnership", source, config, mutations)
    print("Memory participant ownership: 10/10 mutants killed; 5 lifecycle witnesses reachable")
