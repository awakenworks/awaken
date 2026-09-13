"""Deletion-domain qualification over the existing model and trace runner."""
from copy import deepcopy
import json
from pathlib import Path
import tempfile

from formal_model_mutation_runner import check_tlc_model_mutations

ROOT = Path(__file__).resolve().parents[2]


def check_session_deletion_model_mutants(tlc_jar: Path) -> None:
    source = (ROOT / "formal/tla/SessionDeletion.tla").read_text()
    # Safety mutants need no fairness. The normal and Unconfirmed configs own
    # temporal recovery and permanent uncertainty; do not duplicate that run.
    config = "SPECIFICATION Spec\nCONSTANTS MaxCleanupFailures = 2 MaxRevision = 5\nINVARIANTS DeleteSafety\nCHECK_DEADLOCK FALSE\n"
    replay = source[source.index("ReplayDelete =="):source.index("\\* Delivery acknowledgement")]
    variables = source.split("vars == <<", 1)[1].split(">>", 1)[0]
    variables = [name.strip() for name in variables.replace("\n", "").split(",")]

    def corrupted_replay(field: str, value: str) -> str:
        # Mutate the one existing action, not a second protocol. The complete
        # unchanged frame retains every other modeled axis for a causal kill.
        frame = ", ".join(name for name in variables if name != field)
        action = ('ReplayDelete ==\n    /\\ disposition = "deleting"\n'
                  f"    /\\ {field}' = {value}\n    /\\ UNCHANGED <<{frame}>>\n\n")
        return source.replace(replay, action, 1)

    mutants = []
    for field, value, invariant in [
        ("visible", "TRUE", "HiddenDeleteStateIsTerminal"),
        ("deletedFactCount", "deletedFactCount + 1", "DeletedFactIsCommittedAtMostOnce"),
        ("cleanupEffectId", '"foreign"', "CleanupUsesStableEffectId"),
    ]:
        mutants.append((corrupted_replay(field, value),
            config.replace("INVARIANTS DeleteSafety", f"INVARIANTS {invariant}"), invariant))
    tombstone = source[source.index("CommitTombstone =="):source.index("Crash ==")]
    reemit = tombstone.replace("deletedFactCount, deletedFactPending,", "deletedFactCount,", 1)
    reemit = reemit.replace("    /\\ revision' = revision + 1", "    /\\ deletedFactPending' = TRUE\n    /\\ revision' = revision + 1", 1)
    mutants.append((source.replace(tombstone, reemit, 1), config + "PROPERTIES NoFactAfterDeleteFence\n", "action:NoFactAfterDeleteFence"))

    # One conceptual regression: DELETE overwrites an already-owned archive
    # operation with a fresh fence. Both action and its observation admission
    # are weakened; the independent temporal frame must reject the rewrite.
    intent = source[source.index("CommitDeleteIntent =="):source.index("\\* Reissuing DELETE")]
    start = intent.index('    /\\ IF cleanupPhase = "not_requested"')
    end = intent.index("    /\\ visible'", start)
    reset = intent[:start] + ('    /\\ cleanupPhase\' = "fenced" /\\ cleanupEffectId\' = DeleteEffect\n'
        '    /\\ UNCHANGED <<lastAttemptEffectId, cleanupAttempts, cleanupFailures, resourceReleaseRequested>>\n') + intent[end:]
    reset = reset.replace("    /\\ DeleteFence(DeleteView, DeleteView')\n", "")
    mutants.append((source.replace(intent, reset, 1), config + "PROPERTIES ArchiveCleanupSurvivesDelete\n", "action:ArchiveCleanupSurvivesDelete"))

    # Retain the existing physical-evidence mutants in this same owner.
    prepare = source[source.index("PrepareParticipant(participant, effect) =="):source.index("RemoveTarget(target, effect) ==")]
    remove = source[source.index("RemoveTarget(target, effect) =="):source.index("Next ==")]
    success = source[source.index("CleanupSucceeds =="):source.index("CommitTombstone ==")]
    physical = config.replace("INVARIANTS DeleteSafety", "INVARIANTS PhysicalEvidenceRequired")
    for mutant in [
        source.replace(prepare, prepare.replace("~physicalActive /\\ effect", "effect", 1)),
        source.replace(remove, remove.replace("~physicalActive /\\ prepared = Participants", "TRUE", 1)),
        source.replace(success, success.replace("removedTargets = PhysicalTargets", "removedTargets # {}", 1)),
        source.replace(success, success.replace("/\\ ~physicalActive /\\ prepared = Participants\n                         /\\ removedTargets = PhysicalTargets", "", 1)),
    ]:
        mutants.append((mutant, physical, "PhysicalEvidenceRequired"))
    check_tlc_model_mutations(tlc_jar, "SessionDeletion", source, config, mutants, dependencies=("SessionCleanupKernel",))

    # Positive action witnesses: each archive phase must actually reach DELETE.
    # A blanket no-archive-delete restriction would satisfy safety vacuously.
    witnesses = []
    for phase in ("fenced", "requested", "completed"):
        name = "NoArchiveDelete" + phase.title()
        operator = f'{name} == [][~(disposition = "archived" /\\ disposition\' = "deleting" /\\ cleanupPhase = "{phase}")]_vars\n'
        witness = source.replace("\n=============================================================================", "\n" + operator + "=============================================================================")
        witnesses.append((witness, config + f"PROPERTIES {name}\n", "action:" + name))
    name = "NoAcknowledgedTombstone"
    operator = f'{name} == ~(disposition = "tombstoned" /\\ deletedFactCount = 1 /\\ ~deletedFactPending)\n'
    witness = source.replace("\n=============================================================================", "\n" + operator + "=============================================================================")
    witnesses.append((witness, config.replace("INVARIANTS DeleteSafety", f"INVARIANTS {name}"), name))
    check_tlc_model_mutations(tlc_jar, "SessionDeletion", source, config, witnesses, dependencies=("SessionCleanupKernel",))
    # H1-H4 historical equality cannot omit terminal/fence/exact-instance facts.
    # The same RecoverParticipant action is used by restart/failure/deletion;
    # no second lifecycle model. A positive historical witness prevents a
    # blanket-reject implementation from satisfying this boundary vacuously.
    guard = "    current \\/ (historical /\\ terminal /\\ fenced)\n"
    assert source.count(guard) == 1
    historical_config = config + "PROPERTIES HistoricalObservationSafety\n"
    historical_mutants = [
        (source.replace(guard, "    " + replacement + "\n", 1), historical_config, "action:HistoricalObservationSafety")
        for replacement in ["current \\/ (historical /\\ fenced)", "current \\/ (historical /\\ terminal)", "current \\/ (terminal /\\ fenced)"]
    ]
    historical_mutants.append((source.replace("    /\\ exactInstance\n", "    /\\ TRUE\n", 1),
                               historical_config, "action:HistoricalObservationSafety"))
    check_tlc_model_mutations(tlc_jar, "SessionDeletion", source, historical_config,
                             historical_mutants, dependencies=("SessionCleanupKernel",))
    name = "NoHistoricalRecovery"
    operator = '\nNoHistoricalRecovery == [][~(\\E p \\in Participants: RecoverParticipantObserved(p, FALSE, TRUE, TRUE, TRUE, TRUE))]_vars\n'
    witness = source.replace("\n=============================================================================", operator + "=============================================================================")
    check_tlc_model_mutations(tlc_jar, "SessionDeletion", source, config,
        [(witness, config + f"PROPERTIES {name}\n", "action:" + name)], dependencies=("SessionCleanupKernel",))
    print("Session deletion: 9 lifecycle mutants and 4 lifecycle witnesses; 4 historical-observation mutants and 1 historical witness qualified")


def check_deletion_observation_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    from render_runtime_refinement_traces import render_observations, module_name
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    name = "delete-retry-lifecycle"
    document = json.loads((trace_dir / f"{name}.json").read_text())
    # Causes: corrupt one measured effect at first fence / live replay / ACKed
    # replay / completed-archive delete / tombstone. Each independent corruption
    # must violate the SAME relation that accepted the complete production trace.
    cases = [
        ("active-false-fence", "facts", 0),
        ("archived-completed-false-fence", "cleanup", "rewritten-cleanup"),
        ("active-false-first", "root", "rewritten-root"),
        ("active-false-first", "revision", 99),
        ("active-false-requested", "effect", "foreign"),
        ("active-false-completed", "facts", 1),
        ("active-false-tombstone", "outbox", "duplicated-event"),
        ("active-true-fence", "result", "applied"),
        ("active-false-first", "result", "applied"),
        ("active-false-fence", "receiptOwner", "foreign"),
        ("active-false-first", "receiptRoot", "stale-root"),
        ("active-false-tombstone", "tombstone", "null"),
        ("race-active-same_snapshot-111-retry", "result", "applied"),
        ("race-active-different_revision-111-retry", "receiptRoot", "stale-candidate"),
        ("race-active-after_ack-111-retry", "outbox", "re-enqueued-acknowledged-fact"),
        ("race-active-after_tombstone-111-retry", "tombstone", "null"),
    ]
    with tempfile.TemporaryDirectory(prefix="awaken-deletion-observations-") as temp:
        directory = Path(temp)
        raw, rendered = directory / "raw", directory / "rendered"
        raw.mkdir()
        path = raw / f"{name}.json"
        path.write_text(json.dumps(document))
        render_observations(raw, rendered, ROOT, manifest, False)
        module = module_name(name)
        baseline = (rendered / f"{module}.tla").read_text()
        config = (rendered / f"{module}.cfg").read_text()
        mutants = []
        for rule, field, value in cases:
            mutant = deepcopy(document)
            step = next(step for step in mutant["steps"] if step["rule"] == rule)
            target = step if field in {"result", "receiptOwner", "receiptRoot"} else step["after"]
            assert target[field] != value
            target[field] = value
            path.write_text(json.dumps(mutant))
            render_observations(raw, rendered, ROOT, manifest, False)
            mutants.append(((rendered / f"{module}.tla").read_text(), config, "constant:TraceRefines"))
        check_tlc_model_mutations(tlc_jar, module, baseline, config, mutants,
            dependencies=("SessionDeletion", "SessionCleanupKernel"))
        # Each recorded cause comes from the paused actual request or the
        # real SQLite result. Corrupting one must not keep the same case label.
        for field, value in [("candidateRevision", 99), ("winnerRevision", 99),
                ("sqliteResult", "conflict"), ("winnerTimestamp", 222),
                ("delayedTimestamp", 111), ("acknowledged", True), ("tombstoned", True)]:
            mutant = deepcopy(document)
            step = next(step for step in mutant["steps"] if step["rule"] == "race-active-same_snapshot-111-retry")
            assert step["race"][field] != value
            step["race"][field] = value
            path.write_text(json.dumps(mutant))
            try:
                render_observations(raw, rendered, ROOT, manifest, False)
            except ValueError as error:
                if "rule conditions drift" not in str(error):
                    raise
            else:
                raise AssertionError(f"deletion race cause mutant survived: {field}")
    print("Session deletion: 16 actual-observation and 7 race-cause mutants qualified")
