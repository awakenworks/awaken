"""Qualify the existing ToolResultProtocol actions and actual batch retry trace."""
from copy import deepcopy
import json
from pathlib import Path
import tempfile

from formal_model_mutation_runner import check_tlc_model_mutations

ROOT = Path(__file__).resolve().parents[2]


def check_event_batch_model_mutants(tlc_jar: Path) -> None:
    source = (ROOT / "formal/tla/ToolResultProtocol.tla").read_text()
    # Keep the same protocol and every batch outcome. Smaller unrelated tool
    # domains avoid multiplying the independent ToolReply state space per mutant.
    config = """SPECIFICATION Spec
CHECK_DEADLOCK FALSE
CONSTANTS Calls = {call1} PendingCommits = {commit1} Operations = {op1}
 Keys = {key1} Fingerprints = {fingerprint1, fingerprint2} None = no_operation
 MaxRejected = 1 MaxConflicts = 1
INVARIANTS EveryBatchKeyAppendsAtMostOnce
"""
    append = source[source.index("AppendBatch(key, fingerprint, reply) =="):source.index("\\* One retry decision")]
    replay = source[source.index("ReplayBatch(key, fingerprint) =="):source.index("\\* Final non-admission")]
    conflict = source[source.index("ConflictBatch(key, fingerprint) =="):source.index("End ==")]
    variables = [name.strip() for name in source.split("vars == <<", 1)[1].split(">>", 1)[0].replace("\n", "").split(",")]
    rewritten = replay.replace("    /\\ UNCHANGED vars", "    /\\ batchFingerprint' = [batchFingerprint EXCEPT ![key] = None]\n    /\\ UNCHANGED <<" + ", ".join(name for name in variables if name != "batchFingerprint") + ">>")
    appended = conflict.replace("    /\\ batchConflicts'", "    /\\ batchAppends' = [batchAppends EXCEPT ![key] = @ + 1]\n    /\\ batchConflicts'", 1).replace("batchFingerprint, batchAppends, batchReply>>", "batchFingerprint, batchReply>>", 1)
    mutants = [
        (source.replace(append, append.replace("    /\\ batchFingerprint[key] = None", "    /\\ TRUE", 1), 1), config, "EveryBatchKeyAppendsAtMostOnce"),
        (source.replace(replay, rewritten, 1), config + "PROPERTIES BatchFingerprintNeverChanges\n", "action:BatchFingerprintNeverChanges"),
        (source.replace(conflict, appended, 1), config, "EveryBatchKeyAppendsAtMostOnce"),
    ]
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, config, mutants)
    # Both kinds of retained identity must be reachable: closing all requests
    # or accepting all requests cannot make the retry claims pass vacuously.
    witnesses = []
    for name, condition in [("NoAcceptedBatch", "batchAppends[key1] = 0"), ("NoClosedBatch", "~BatchIsClosed(key1)")]:
        # key1 is a configuration model value, so quantify over the source Keys.
        condition = condition.replace("key1", "key")
        witness = source.replace("Spec == Init", f"{name} == \\A key \\in Keys: {condition}\n\nSpec == Init", 1)
        witnesses.append((witness, config.replace("EveryBatchKeyAppendsAtMostOnce", name), name))
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, config, witnesses)
    print("Event batch: 3 model mutants and 2 retained-outcome witnesses qualified")
    # Admission/consumption are separate boundaries. Two distinct HTTP keys
    # may validate the same ticket before either CAS. Removing the root-owned
    # occupancy guard must create two accepted replies, even before Deliver.
    reply_config = config.replace("Keys = {key1}", "Keys = {key1, key2}").replace(
        "INVARIANTS EveryBatchKeyAppendsAtMostOnce", "INVARIANTS EveryPendingCallHasAtMostOneAdmittedReply")
    guard = "    /\\ reply = None \\/ (\\A retained \\in Keys: batchReply[retained] # reply)"
    assert source.count(guard) == 1
    duplicate = source.replace(guard, "    /\\ TRUE", 1)
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, reply_config,
                             [(duplicate, reply_config, "EveryPendingCallHasAtMostOneAdmittedReply")])
    witness = source.replace("Spec == Init", "NoAdmittedToolReply == \\A key \\in Keys: batchReply[key] = None\n\nSpec == Init", 1)
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, reply_config,
                             [(witness, reply_config.replace("EveryPendingCallHasAtMostOneAdmittedReply", "NoAdmittedToolReply"), "NoAdmittedToolReply")])
    print("Tool result: 1 root occupancy mutant and 1 pre-delivery admission witness qualified")
    # Call/commit decision table: both coordinates exact enables acceptance;
    # either coordinate foreign disables it. Two values per coordinate expose
    # each removed guard independently. Observe the PRE-state awaiting call,
    # otherwise lastAccepted=lastExpected=op masks a missing call guard.
    call_config = """SPECIFICATION Spec
CHECK_DEADLOCK FALSE
CONSTANTS Calls = {call1, call2} PendingCommits = {commit1, commit2} Operations = {op1}
 Keys = {key1} Fingerprints = {fingerprint1} None = no_operation
 MaxRejected = 1 MaxConflicts = 1
INVARIANTS OnlyMatchingResultIsAccepted
"""
    deliver = source[source.index("Deliver(call, pendingCommit, op) =="):source.index("\\* A response-loss retry")]
    call_mutants = []
    for guard in ("    /\\ awaiting = call", "    /\\ awaitingCommit = pendingCommit"):
        assert deliver.count(guard) == 1, "production-model guard must have one owner"
        mutant = source.replace(deliver, deliver.replace(guard, "    /\\ TRUE", 1), 1)
        call_mutants.append((mutant, call_config, "OnlyMatchingResultIsAccepted"))
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, call_config, call_mutants)
    # Positive witness prevents an always-rejecting Deliver from satisfying the
    # safety invariant vacuously: an exact call/commit must be reachable.
    witness = source.replace("Spec == Init", "NoAcceptedCall == lastAcceptedCall = None\n\nSpec == Init", 1)
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, call_config,
                             [(witness, call_config.replace("OnlyMatchingResultIsAccepted", "NoAcceptedCall"), "NoAcceptedCall")])
    print("Tool result: 2 coordinate-guard mutants and 1 exact-call witness qualified")
    # Terminal cause/effect table: End must clear both ticket coordinates;
    # a later consumer must not treat terminal truth as delivery authority.
    # Exact retained receipts must remain replayable after End. Use the SAME
    # model/runner; mutation CFGs isolate the intended semantic oracle.
    terminal_config = call_config.replace("INVARIANTS OnlyMatchingResultIsAccepted", "INVARIANTS TerminalRejectsLateResults ExactResultReceiptReplayRemainsEnabled\nPROPERTIES TerminalConsumptionNeverChanges")
    end = source[source.index("End =="):source.index("Next ==")]
    stale = end.replace("       /\\ awaiting' = None", "       /\\ awaiting' = awaiting", 1)
    guards = "    /\\ ~terminal\n    /\\ awaiting = call\n    /\\ awaitingCommit = pendingCommit"
    assert deliver.count(guards) == 1, "one result-consumption admission owner"
    reopened = deliver.replace(guards, "    /\\ (terminal \\/ (awaiting = call /\\ awaitingCommit = pendingCommit))", 1)
    terminal_mutants = [
        (source.replace(end, stale, 1), terminal_config, "TerminalRejectsLateResults"),
        (source.replace(deliver, reopened, 1), terminal_config, "action:TerminalConsumptionNeverChanges"),
    ]
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, terminal_config, terminal_mutants)
    # A green replay-enabled invariant alone is vacuous if no consumed receipt
    # survives End. This witness requires such a reachable terminal state.
    witness = source.replace("Spec == Init", "NoTerminalReceipt == ~(terminal /\\ completed # {})\n\nSpec == Init", 1)
    witness_config = terminal_config.replace("INVARIANTS TerminalRejectsLateResults ExactResultReceiptReplayRemainsEnabled", "INVARIANTS NoTerminalReceipt")
    check_tlc_model_mutations(tlc_jar, "ToolResultProtocol", source, terminal_config,
                             [(witness, witness_config, "NoTerminalReceipt")])
    print("Tool result: 2 terminal mutants and 1 terminal receipt replay witness qualified")


def check_event_batch_observation_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    from render_runtime_refinement_traces import render_observations, module_name
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    name = "event-batch-recovery-retry"
    document = json.loads((trace_dir / f"{name}.json").read_text())
    # One measured effect is corrupted per cause row; all input classifications
    # and retained evidence stay intact. The same relation must reject all.
    cases = [
        ("exact", ("result",), "unavailable"),
        ("exact", ("receipt",), "wrong-receipt"),
        ("changed-body", ("result",), "exact"),
        ("changed-subject", ("result",), "exact"),
        ("closed", ("result",), "exact"),
        ("absent", ("error",), "replacement-error"),
        ("exact", ("after", "root"), "changed-root"),
        ("changed-body", ("after", "outbox"), "new-fact"),
        ("exact", ("after", "reads"), 999),
        ("exact", ("cache_unchanged",), False),
    ]
    with tempfile.TemporaryDirectory(prefix="awaken-event-batch-trace-") as temp:
        raw, rendered = Path(temp) / "raw", Path(temp) / "rendered"
        raw.mkdir()
        path = raw / f"{name}.json"
        path.write_text(json.dumps(document))
        render_observations(raw, rendered, ROOT, manifest, False)
        module = module_name(name)
        baseline = (rendered / f"{module}.tla").read_text()
        config = (rendered / f"{module}.cfg").read_text()
        mutants = []
        for rule, fields, value in cases:
            mutant = deepcopy(document)
            target = next(step for step in mutant["steps"] if step["rule"] == rule)["observation"]
            for field in fields[:-1]: target = target[field]
            assert target[fields[-1]] != value
            target[fields[-1]] = value
            path.write_text(json.dumps(mutant))
            render_observations(raw, rendered, ROOT, manifest, False)
            mutants.append(((rendered / f"{module}.tla").read_text(), config, "constant:TraceRefines"))
        check_tlc_model_mutations(tlc_jar, module, baseline, config, mutants, dependencies=("ToolResultProtocol",))
    print("Event batch: 10 actual-observation mutants qualified")
