#!/usr/bin/env python3
"""Mutation adequacy of the critical formal models and of the strict-CI wiring.

Model families (one flag each, run by the full tier of check_formal.sh) re-check
a critical authority model's green baseline -- leases, fences, budgets,
credentials, erasure -- then require every guard-removing mutant to fail its
exact named invariant. ``--gate-mutants`` edits a private copy of the formal
gate and requires check_formal_coverage to reject it. The no-argument run is
the oracle's own unit tests (TLC exit/text classification, family selection,
and the runtime-refinement trace renderer), fast enough for every run.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from formal_gate_mutations import must_fail, run_gate_mutations
from formal_model_mutation_runner import check_tlc_model_mutations
from formal_config_mutations import check_config_cas_model_mutant
from formal_erasure_mutations import check_erasure_checkpoint_model_mutants
from formal_credential_mutations import (
    check_mcp_credential_model_mutants, check_credential_effect_model_mutants,
    check_acp_binding_model_mutants,
    check_acp_observation_model_mutants,
    check_acp_projection_model_mutants,
)
from formal_background_mutations import check_background_completion_model_mutant


ROOT = Path(__file__).resolve().parents[2]


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


class MutationOracleTests(unittest.TestCase):
    def test_model_cli_flags_keep_their_exact_single_handler(self) -> None:
        # R1 each existing flag => exactly its handler, once, with parsed Path.
        # This compatibility list is independent of the production registry;
        # a lost registration or mismatched dispatch must fail before real TLC.
        flags = (
            "session-budget-admission-model-mutants", "work-queue-stop-model-mutants",
            "session-ownership-model-mutants", "config-cas-model-mutant",
            "background-completion-model-mutant", "file-reclamation-model-mutants",
            "resource-preparation-model-mutant", "realization-renewal-model-mutant",
            "erasure-checkpoint-model-mutants", "credential-creation-model-mutants",
            "mcp-credential-model-mutants", "credential-effect-model-mutants",
            "acp-binding-model-mutants", "acp-observation-model-mutants",
            "acp-projection-model-mutants",
        )
        for flag in flags:
            handler = "check_" + flag.replace("-", "_")
            with self.subTest(flag=flag), patch(f"{__name__}.{handler}") as check:
                with patch.object(sys, "argv", ["checker", f"--{flag}", "tlc.jar"]): main()
                check.assert_called_once_with(Path("tlc.jar"))

    def test_tlc_launch_keeps_the_resource_bound_and_strict_timeout(self) -> None:
        # R1 default and R2 explicit composed-model budgets reach every child
        # invocation. Both still require a green baseline and the exact named
        # invariant counterexample; no timeout or merely nonzero exit is a kill.
        # R3 budgets/worker count keep the canonical collector and full configuration;
        # R4 a timeout escapes as failed verification, never a semantic kill.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        rejected = subprocess.CompletedProcess([], 12, "Invariant ExactRule is violated")
        for budget, heap, timeout, workers in (({}, "-Xmx512m", 60, "1"),
                                     ({"heap_mb": 2048, "timeout_seconds": 600, "workers": 4}, "-Xmx2048m", 600, "4")):
            with self.subTest(budget=budget), patch("subprocess.run", side_effect=[green, rejected]) as runner:
                check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                          [("mutant", "config", "ExactRule")], **budget)
                self.assertEqual(runner.call_count, 2)
                for call in runner.call_args_list:
                    self.assertEqual(call.args[0][:5], ["java", f'-Djava.io.tmpdir={call.kwargs["cwd"]}', heap, "-XX:+UseParallelGC", "-jar"])
                    self.assertEqual(call.kwargs["timeout"], timeout)
                    argv = call.args[0]
                    self.assertEqual(argv[argv.index("-workers") + 1], workers)
            with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("java", timeout)):
                with self.assertRaises(subprocess.TimeoutExpired):
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config", [], **budget)

    def test_budget_admission_keeps_both_canonical_dependencies_and_causal_rows(self) -> None:
        # B1 A/U/R/E/C use one full baseline, ten unsafe source variants and
        # positive joint-Open/joint-Settle, both caller and same-key witnesses.
        # B2 H1/H2 keep that full domain with one categorical malformed key.
        # B3 anchor drift fails before launching any tool. This is an isolated
        # transport/selection test, NOT TLC execution or a model-safety result.
        source = (ROOT / "formal/tla/SessionRunBudgetAdmission.tla").read_text()
        config = (ROOT / "formal/tla/SessionRunBudgetAdmission.cfg").read_text()
        # B5 imported tuple operators parse, but do not expose local variables
        # as assignable UNCHANGED frames. Require direct local tuples covering
        # every canonical variable (with the explicit Activity renaming); the
        # real TLC baseline remains the execution oracle for the joint actions.
        def tuple_names(text: str, marker: str) -> list[str]:
            return [name.strip() for name in text.split(marker, 1)[1].split(">>", 1)[0].split(",")]

        cas = (ROOT / "formal/tla/AggregateCAS.tla").read_text()
        self.assertEqual(tuple_names(source, "casVars == <<"), tuple_names(cas, "vars == <<"))
        kernel = (ROOT / "formal/tla/SessionActivityKernel.tla").read_text()
        import re
        instance = source.split("Activity == INSTANCE SessionActivityKernel WITH", 1)[1].split("casVars ==", 1)[0]
        renamed = dict(re.findall(r"(\w+)\s*<-\s*(\w+)", instance))
        self.assertEqual(tuple_names(source, "activityVars == <<"),
                         [renamed[name] for name in tuple_names(kernel, "kVars == <<")])
        with patch(f"{__name__}.check_tlc_model_mutations") as runner:
            check_session_budget_admission_model_mutants(Path("tlc.jar"))
        self.assertEqual(runner.call_count, 3)
        first, positive, negative = runner.call_args_list
        self.assertEqual(first.args[:4], (Path("tlc.jar"), "SessionRunBudgetAdmission", source, config))
        for call in runner.call_args_list:
            self.assertEqual(call.kwargs, {"dependencies": ("SessionActivityKernel", "AggregateCAS")})
            self.assertIn("Operations = {operation_a, operation_b, operation_c}", call.args[3])
            self.assertIn("Callers = {fast, worker_repair}", call.args[3])
            self.assertIn("MaxRootChanges = 3", call.args[3])
            self.assertIn("MaxActivityEpoch = 4", call.args[3])
        self.assertEqual(len(first.args[4]), 27)
        self.assertTrue(all(mutant != source for mutant, _, _ in first.args[4][:10]))
        self.assertEqual([oracle for _, _, oracle in first.args[4][:10]], [
            "AcceptedObservationIsNotDiscarded", "NoAcceptedNegativePair", "HealthyClosedReadReplaysAccepted",
            "action:NewRefusalWasBudgetRejected", "action:RejectedCASPreservesDurableState",
            "action:ActivityCommitUsesExactRootRevision", "UncertaintyCannotReportRefusal",
            "UncertaintyCannotReportRefusal", "RaisedDecisionCannotCloseRefusal", "action:NegativeReceiptIsAbsorbing",
        ])
        for _, cfg, oracle in first.args[4][:10]:
            selected = [line for line in cfg.splitlines() if line.startswith(("INVARIANTS ", "PROPERTIES "))]
            self.assertEqual(selected, [f"PROPERTIES {oracle.removeprefix('action:')}" if oracle.startswith("action:") else f"INVARIANTS {oracle}"])
        witnesses = ("NoOpenCommit", "NoSettleCommit", "NoSettledReplay", "NoAcceptedAfterStaleBudget",
                     "NoRaisedPending", "NoNegativeAfterRaise", "NoNegativeCASLossToRaise",
                     "NoNegativeCASLossToAccepted", "NoBeforeWriteOutage", "NoNegativeLostAckRecovery",
                     "NoPositiveLostAckRecovery", "NoFastOpenCommit", "NoWorkerOpenCommit",
                     "NoFastNegativeCommit", "NoWorkerNegativeCommit", "NoPositiveStoreReplay", "NoNegativeStoreReplay")
        self.assertEqual(first.args[4][10:], [
            (source, config.replace("INVARIANTS Safety", f"INVARIANTS {name} Safety"), name)
            for name in witnesses
        ])
        for call, kind in ((positive, "Positive"), (negative, "Negative")):
            self.assertEqual(call.args[3], config.replace('ReceiptFault = "None"', f'ReceiptFault = "{kind}"'))
            self.assertEqual(len(call.args[4]), 2)
            self.assertEqual(call.args[4][0][2], "CorruptReceiptNeverBecomesAbsence")
            self.assertEqual(call.args[4][1][2], f"No{kind}HashRefusal")
        with patch.object(Path, "read_text", side_effect=[source.replace("    /\\ IF lastApplied'\n", "    /\\ IF TRUE\n"), config]), \
                patch(f"{__name__}.check_tlc_model_mutations") as runner:
            with self.assertRaisesRegex(SystemExit, "Budget admission mutation anchor drifted"):
                check_session_budget_admission_model_mutants(Path("tlc.jar"))
            runner.assert_not_called()

    def test_observation_time_embedding_preserves_and_reflects_relations(self) -> None:
        from render_runtime_refinement_traces import normalize_observation_times
        # Q4 TLC's integer bound must not truncate, saturate or conflate actual
        # clocks. Equal/order/max outcomes must agree for every pair, including
        # legacy None, zero, i32 overflow and u64::MAX. Non-time cursors stay exact.
        values = [0, 1, 2**31, 1788710583713, 2**64 - 1]
        steps = [{"before": {"time": a, "sequence": 7}, "after": {"time": b}, "now": max(a, b)} for a in values for b in values]
        mapped = normalize_observation_times(steps)
        for original, projected in zip(steps, mapped):
            a, b = original["before"]["time"], original["after"]["time"]
            x, y = projected["before"]["time"], projected["after"]["time"]
            self.assertEqual(a == b, x == y)
            self.assertEqual(a < b, x < y)
            self.assertEqual(max(x, y), projected["now"])
            self.assertEqual(projected["before"]["sequence"], 7)
        self.assertEqual(normalize_observation_times([{"time": None}]), [{"time": None}])
        for invalid in [-1, True, "100"]:
            with self.assertRaises(ValueError):
                normalize_observation_times([{"time": invalid}])

    def test_realization_root_and_reference_keys_name_one_complete_generation(self) -> None:
        from render_runtime_refinement_traces import project_realization_trace
        # G1 the root and every directive/call/report name a generation with the
        # SAME key, emitted once by the Rust observation port: this renderer
        # validates and uses it, and never rebuilds a second one from parts.
        # G2 a different content digest (or version, or attachment) is another
        # generation, never an alias for the current one. G3 an undecodable,
        # truncated, widened or mistyped key is shape drift, not an opaque name
        # that can never be authorized.
        key = json.dumps(["mcp_a", 2, "fnv1a64:1"], separators=(",", ":"))
        lease = {"owner": "worker", "runtime_incarnation": "inc", "epoch": 1, "expires_at_unix_ms": 9}
        def trace(root_key, reference_key):
            root = {"sequence": 0, "session": "s", "point": "begin-before", "data": {"revision": 1, "created": 1, "updated": 1, "lease": lease,
                "attachments": [{"key": root_key, "state": "requested", "ack": False, "claim": None}]}}
            reference = {"key": reference_key, "session_id": "s", "runtime_incarnation": "inc", "lease_epoch": 1, "lease_expires_at_unix_ms": 9}
            directive = {"sequence": 1, "session": "s", "point": "projection", "data": {"lease": lease, "action": {"phase": "stage", "generations": [reference]}}}
            return project_realization_trace([root, directive])
        projected = trace(key, key)[1]
        self.assertEqual(projected["pending"], projected["expectstage"], "G1")
        self.assertEqual(projected["expectstage"], [key], "G1 the emitted key is used verbatim")
        for axis, forged in (("content_digest", ["mcp_a", 2, "fnv1a64:2"]),
                             ("version", ["mcp_a", 3, "fnv1a64:1"]),
                             ("attachment_id", ["mcp_b", 2, "fnv1a64:1"])):
            with self.subTest(axis=axis):
                projected = trace(key, json.dumps(forged, separators=(",", ":")))[1]
                self.assertNotEqual(projected["pending"], projected["expectstage"], "G2")
        malformed = ('["mcp_a",2]', "not-json", "", '["mcp_a","2","fnv1a64:1"]',
                     '["mcp_a",2,"fnv1a64:1","extra"]', '{"attachment_id":"mcp_a"}')
        for raw_key in malformed:
            with self.subTest(raw_key=raw_key, side="root"), self.assertRaisesRegex(ValueError, "observation schema violation"):
                trace(raw_key, key)
            with self.subTest(raw_key=raw_key, side="reference"), self.assertRaisesRegex(ValueError, "observation schema violation"):
                trace(key, raw_key)

    def test_observation_trace_gate_rejects_missing_scenarios_and_schema_drift(self) -> None:
        from render_runtime_refinement_traces import render_observations
        manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
        # Q3 fresh empty output, unknown fields, or incomplete production
        # scenarios cannot retain green coverage from a prior invocation.
        with tempfile.TemporaryDirectory() as temp:
            raw, output = Path(temp) / "raw", Path(temp) / "rendered"
            raw.mkdir()
            with self.assertRaises(ValueError):
                render_observations(raw, output, ROOT, manifest, True)
            write_json(raw / "bad.json", {"schema_version": 1})
            with self.assertRaises(ValueError):
                render_observations(raw, output, ROOT, manifest, False)
            (raw / "bad.json").unlink()
            # Q5 a present name with another family's relation cannot count as
            # the required producer. Q6 a valid partial run must still fail the
            # complete gate, even when the output directory already exists.
            document = {"schema_version": 4, "projection_id": "session-observations-v4", "name": "binding-apply", "family": "processed", "steps": [{}]}
            write_json(raw / "binding-apply.json", document)
            with self.assertRaisesRegex(ValueError, "scenario/family drift"):
                render_observations(raw, output, ROOT, manifest, False)
            document["family"] = "root"
            document["steps"] = [{"rule": "binding-apply", "before": {"exists": True, "revision": 1, "created": 100, "updated": 100}, "after": {"exists": True, "revision": 2, "created": 100, "updated": 200}, "now": 200}]
            write_json(raw / "binding-apply.json", document)
            with self.assertRaisesRegex(ValueError, "coverage is incomplete"):
                render_observations(raw, output, ROOT, manifest, True)


    def test_temporal_mutation_requires_exact_exit_and_a_single_property(self) -> None:
        # T1 green base + exit 13 + temporal violation + one named property is
        # a kill. T2 unrelated safety/parser/tool failures cannot count. T3 a
        # different/multiple property selection cannot use TLC's generic text.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for config, code, output, accepted in (
            ("PROPERTIES Releases", 13, "Temporal properties were violated.", True),
            ("PROPERTIES Releases", 12, "Invariant Safety is violated", False),
            ("PROPERTIES Releases", 1, "parse error", False),
            ("PROPERTIES Releases", 0, "Temporal properties were violated.", False),
            ("PROPERTIES Releases Other", 13, "Temporal properties were violated.", False),
            ("PROPERTIES Other", 13, "Temporal properties were violated.", False),
        ):
            with self.subTest(config=config, code=code), patch("subprocess.run", side_effect=[
                green, subprocess.CompletedProcess([], code, output),
            ]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", config,
                                              [("mutant", config, "temporal:Releases")])
                else:
                    with self.assertRaises(SystemExit):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", config,
                                                  [("mutant", config, "temporal:Releases")])

    def test_action_mutation_requires_exact_named_property_and_exit(self) -> None:
        # A1 exact named action violation + exit 13 => kill; A2 wrong name,
        # generic temporal text, parser error or successful exit => not a kill.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in [(13, "Action property Stable is violated.", True),
            (13, "Action property Other is violated.", False), (13, "Temporal properties were violated.", False),
            (1, "parse error", False), (0, "Action property Stable is violated.", False)]:
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[green, subprocess.CompletedProcess([], code, output)]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "PROPERTY Stable", [("mutant", "PROPERTY Stable", "action:Stable")])
                else:
                    with self.assertRaises(SystemExit):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "PROPERTY Stable", [("mutant", "PROPERTY Stable", "action:Stable")])

    def test_tlc_mutations_require_a_green_base_and_exact_invariant_failure(self) -> None:
        # TLC cause/effect table: baseline green x mutant exit x invariant text.
        # Only green+12+exact invariant passes. Infrastructure/parser failures,
        # unrelated invariants and surviving mutants must not count as kills.
        # Isolation rule: each invocation extracts standard modules inside its
        # own temporary owner; a peer exit cannot delete this JVM's parser input.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in (
            (12, "Invariant ExactRule is violated", True),
            (0, "Invariant ExactRule is violated", False),
            (12, "Invariant OtherRule is violated", False),
            (1, "parse error", False),
            (150, "NullPointerException: parsing failed", False),
            (-9, "Invariant ExactRule is violated", False),
        ):
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[
                green, subprocess.CompletedProcess([], code, output),
            ]) as runner:
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                              [("mutant", "config", "ExactRule")])
                else:
                    with self.assertRaisesRegex(SystemExit, "not killed correctly"):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                                  [("mutant", "config", "ExactRule")])
                calls = runner.call_args_list
                self.assertEqual(len({call.kwargs["cwd"] for call in calls}), 2)
                for call in calls: self.assertIn(f'-Djava.io.tmpdir={call.kwargs["cwd"]}', call.args[0])
        with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 12,
                "Invariant ExactRule is violated")) as runner:
            with self.assertRaisesRegex(SystemExit, "baseline failed"):
                check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                          [("mutant", "config", "ExactRule")])
            self.assertEqual(runner.call_count, 1, "a failed base cannot reach its mutant")

    def test_only_the_expected_semantic_rejection_is_a_kill(self) -> None:
        # G1-G5: C1 exit=0/1/signal/parser error; C2 expected rule matches;
        # C3 infrastructure traceback. Only exit=1+C2+!C3 is a killed mutant.
        for code, stderr, killed in (
            (0, "expected rule", False), (1, "expected rule", True),
            (1, "unrelated missing model", False), (-9, "expected rule", False),
            (2, "expected rule", False), (1, "Traceback: expected rule", False),
        ):
            with self.subTest(code=code, stderr=stderr), patch(
                "formal_gate_mutations.run_checker",
                return_value=subprocess.CompletedProcess([], code, "", stderr),
            ):
                if killed:
                    must_fail(ROOT, "checker.py", "oracle", expected="expected rule")
                else:
                    with self.assertRaises(SystemExit):
                        must_fail(ROOT, "checker.py", "oracle", expected="expected rule")

    def test_unavailable_or_timed_out_tool_is_not_a_kill(self) -> None:
        # G5: missing executable and timeout fail before semantic classification.
        for error in (OSError("missing tool"), subprocess.TimeoutExpired("checker", 120)):
            with self.subTest(error=error), patch("subprocess.run", side_effect=error):
                with self.assertRaisesRegex(SystemExit, "infrastructure failure"):
                    must_fail(ROOT, "checker.py", "oracle", expected="expected rule")


def main() -> None:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    # One registry owns both accepted CLI flags and their execution handlers.
    model_checks = {
        "session-budget-admission-model-mutants": check_session_budget_admission_model_mutants,
        "work-queue-stop-model-mutants": check_work_queue_stop_model_mutants,
        "session-ownership-model-mutants": check_session_ownership_model_mutants,
        "config-cas-model-mutant": check_config_cas_model_mutant,
        "background-completion-model-mutant": check_background_completion_model_mutant,
        "file-reclamation-model-mutants": check_file_reclamation_model_mutants,
        "resource-preparation-model-mutant": check_resource_preparation_model_mutant,
        "realization-renewal-model-mutant": check_realization_renewal_model_mutant,
        "erasure-checkpoint-model-mutants": check_erasure_checkpoint_model_mutants,
        "credential-creation-model-mutants": check_credential_creation_model_mutants,
        "mcp-credential-model-mutants": check_mcp_credential_model_mutants,
        "credential-effect-model-mutants": check_credential_effect_model_mutants,
        "acp-binding-model-mutants": check_acp_binding_model_mutants,
        "acp-observation-model-mutants": check_acp_observation_model_mutants,
        "acp-projection-model-mutants": check_acp_projection_model_mutants,
    }
    selection = parser.add_mutually_exclusive_group()
    for name in model_checks:
        selection.add_argument(f"--{name}", type=Path, metavar="TLC_JAR")
    selection.add_argument("--gate-mutants", action="store_true",
                           help="mutate the strict-CI wiring check_formal_coverage reads")
    arguments = parser.parse_args()
    if arguments.gate_mutants:
        run_gate_mutations()
        return
    for name, check in model_checks.items():
        if (jar := getattr(arguments, name.replace("-", "_"))) is not None:
            check(jar)
            return
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(MutationOracleTests)
    if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
        raise SystemExit(1)


def check_resource_preparation_model_mutant(tlc_jar: Path) -> None:
    # P1 omit early lease visibility (the reproduced bug); P2 let a cancelled
    # preparation publish. Both must violate the named authority invariant,
    # not fail due to syntax, timeout, or infrastructure. Reuse the model/oracle.
    source = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/SessionRealizationMutex.cfg").read_text(encoding="utf-8")
    config = config.replace("MaxCompleted = 3", "MaxCompleted = 1").replace("MaxRenewals = 6", "MaxRenewals = 1")
    visible = "  /\\ leaseVisible' = [leaseVisible EXCEPT ![session] = TRUE]"
    publish = '  /\\ phase[session] = "PreparingResources"\n  /\\ leaseVisible[session]'
    if source.count(visible) != 1 or source.count(publish) != 1:
        raise SystemExit("Resource preparation mutation anchor drifted; review the model")
    mutants = [
        source.replace(visible, visible.replace("TRUE", "FALSE")),
        source.replace(publish, '  /\\ phase[session] \\in {"PreparingResources", "CancellingResources"}'),
    ]
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", source, config,
        [(mutant, config, "ResourcesHaveRenewableAuthority") for mutant in mutants])
    print("Resource preparation mutation score: 2/2 killed (hidden lease, cancelled publication)")


def check_file_reclamation_model_mutants(tlc_jar: Path) -> None:
    # F1 release ownership before the tombstone; F2 fence while that ownership
    # remains. Require named safety failures. W1/W2 prove both separate durable
    # prefixes can survive a crash; W3 proves receipt completion is reachable.
    # These witnesses reject the old atomic-delete abstraction, not SQL engines.
    source = (ROOT / "formal/tla/ResourceReclamation.tla").read_text()
    config = (ROOT / "formal/tla/ResourceReclamation.cfg").read_text()
    release = 'ReleaseFileOwnership ==\n    /\\ processUp\n    /\\ lifecycle = "deleted"'
    fence = 'AcquireFence ==\n    /\\ processUp\n    /\\ intentState = "claimed"\n    /\\ ~fileOwnership'
    if source.count(release) != 1 or source.count(fence) != 1:
        raise SystemExit("File reclamation mutation anchors drifted")
    mutations = [
        (source.replace(release, release.replace('lifecycle = "deleted"', 'TRUE')), config, "ActiveFileRetainsOwnership"),
        (source.replace(fence, fence.replace('~fileOwnership', 'TRUE')), config, "FenceHasNoReferencesOrLeases"),
    ]
    for name, condition in [
        ("IntentPrefixUnreachable", 'intentState = "pending" /\\ lifecycle = "active" /\\ ~processUp'),
        ("TombstonePrefixUnreachable", 'lifecycle = "deleted" /\\ fileOwnership /\\ ~processUp'),
        ("CompletionUnreachable", 'receipt /\\ ~fileOwnership'),
    ]:
        witness_source = source.replace("Spec ==", f"{name} == ~({condition})\n\nSpec ==")
        mutations.append((witness_source, config + f"INVARIANT {name}\n", name))
    check_tlc_model_mutations(tlc_jar, "ResourceReclamation", source, config, mutations)
    print("File reclamation: 2/2 mutants killed; 3/3 durable-prefix/completion witnesses reachable")


def check_realization_renewal_model_mutant(tlc_jar: Path) -> None:
    # L1: restore expiry-equality catch-up after a completed phase. Successful
    # renewals forever must expose a non-releasing cycle, not an oracle error.
    source = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationRenewal.cfg").read_text()
    anchor = "  /\\ phase' = [phase EXCEPT ![session] = \"Committing\"]"
    if source.count(anchor) != 1:
        raise SystemExit("Realization renewal mutation anchor drifted; review the model")
    mutant = source.replace(anchor,
        "  /\\ phase' = [phase EXCEPT ![session] = IF receiptCurrent[session] THEN \"Committing\" ELSE \"Driving\"]")
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", source, config,
        [(mutant, config, "temporal:EveryAcquiredSlotEventuallyReleases")])
    print("Realization renewal mutation score: 1/1 killed (continuous-renewal livelock)")
    # Authority A1-A8: remove each decisive guard separately; exact named
    # invariants, not parser/runtime errors, must reject the counterexample.
    # Two inverted reachability witnesses require actual grant and cleanup.
    source = (ROOT / "formal/tla/SessionRealizationAuthority.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationAuthority.cfg").read_text()
    anchors = [
        ('IF projection = "Live" /\\ lease = request', 'IF lease = request',
         "ClosedGenerationCannotRenew"),
        ('Grant ==\n  /\\ UNCHANGED selectionVars\n  /\\ projection = "Live"',
         'Grant ==\n  /\\ UNCHANGED selectionVars\n  /\\ TRUE',
         "NoGrantAfterClose"),
        ('admitted == projection = "Closed" /\\ lease = cleanup[c]',
         'admitted == TRUE', "OldCleanupCannotTouchReplacement"),
        ('/\\ slotIdentity = selectedSlot[c]', '/\\ TRUE', "OldCleanupCannotTouchReplacement"),
        ('workerClosed\' = TRUE /\\ peerLive\' = FALSE',
         'workerClosed\' = TRUE /\\ UNCHANGED peerLive', "WorkerCloseIncludesPeer"),
        ('FinishPeer ==\n  /\\ workerClosed', 'FinishPeer ==\n  /\\ ~ownerLocked /\\ workerClosed',
         "temporal:PeerEventuallyCleans"),
    ]
    mutations = []
    for guard, broken, invariant in anchors:
        if source.count(guard) != 1:
            raise SystemExit("Session authority mutation anchor drifted")
        mutations.append((source.replace(guard, broken), config, invariant))
    for invariant in ("NoGrantReached", "NoCleanupReached"):
        mutations.append((source, config + "\nINVARIANT " + invariant + "\n", invariant))
    check_tlc_model_mutations(tlc_jar, "SessionRealizationAuthority", source, config, mutations)
    print("Session authority: 6/6 guard mutants killed; grant/cleanup reachability witnessed")
    # H1/H2 ordinary and panicking heartbeat completion must use the SAME
    # cohort fence. H3 the wrapper cannot finish ahead of revocation. Exact
    # semantic oracles, plus panic/completion reachability, exclude inert proofs.
    source = (ROOT / "formal/tla/WorkerAuthorityExit.tla").read_text()
    config = (ROOT / "formal/tla/WorkerAuthorityExit.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ("  /\\ CloseWorker\n", '  /\\ IF kind = "panicked" THEN UNCHANGED vars ELSE CloseWorker\n',
         "HeartbeatExitFencesCohort"),
        ("  /\\ CloseWorker\n", '  /\\ IF kind = "returned" THEN UNCHANGED vars ELSE CloseWorker\n',
         "HeartbeatExitFencesCohort"),
        ('/\\ exitPhase = "revoking" /\\ cleaned /\\ peerCleaned /\\ ~ownerLocked',
         '/\\ exitPhase = "revoking"', "CompletionRequiresRevocation"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Worker authority exit mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoPanickedRevocationReached", "NoCompletedRevocationReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "WorkerAuthorityExit", source, config, mutations,
                              dependencies=("SessionRealizationAuthority",))
    print("Worker authority exit: 3/3 mutants killed; panic/completion reachability witnessed")
    # W1-W11 and A1-A6: one root proof, request-start latency, irreversible
    # closure and atomic Session installation. Each removed guard must produce
    # the exact semantic counterexample; admission/renewal witnesses exclude
    # configurations that deny every useful action.
    source = (ROOT / "formal/tla/WorkerExecutionAuthority.tla").read_text()
    config = (ROOT / "formal/tla/WorkerExecutionAuthority.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ('(rootPhase = "Starting" \\/ RootCurrent)',
         '(rootPhase # "Closed")', "ReceiptNeverRevivesExpiredProof"),
        ('(rootPhase = "Starting" \\/ RootCurrent)',
         'TRUE', "ClosedRootNeverReopens"),
        ("/\\ observed < heartbeatRequest + 2", "/\\ TRUE",
         "ReceiptNeverRevivesExpiredProof"),
        ("/\\ observed >= heartbeatRequest", "/\\ TRUE",
         "ReceiptNeverRevivesExpiredProof"),
        ("InstallReplacement ==\n  /\\ RootCurrent /\\ Replace",
         "InstallReplacement ==\n  /\\ Replace", "SessionInstallationRequiresCurrentRoot"),
        ("FreshAdmission ==\n  /\\ RootCurrent /\\ admissions < 2",
         "FreshAdmission ==\n  /\\ admissions < 2", "SessionInstallationRequiresCurrentRoot"),
        ("RenewProjection ==\n  /\\ RootCurrent /\\ ApplyRenewal",
         "RenewProjection ==\n  /\\ ApplyRenewal", "SessionInstallationRequiresCurrentRoot"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Worker execution authority mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoAdmissionReached", "NoRenewedRootReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    # The negative elapsed branch must be reachable, not merely a guard over
    # impossible inputs. NoRegressionResponse is an action property, so the
    # existing oracle demands its named temporal counterexample.
    mutations.append((source, header + "PROPERTY NoRegressionResponse", "action:NoRegressionResponse"))
    check_tlc_model_mutations(tlc_jar, "WorkerExecutionAuthority", source, config, mutations,
                              dependencies=("SessionRealizationAuthority",))
    print("Worker execution authority: 7/7 mutants killed; admission/renewal/regression reachable")


def check_credential_creation_model_mutants(tlc_jar: Path) -> None:
    # M1 absence guard omitted => residual material after WAL retirement.
    # M2 legacy zero advances to reserved epoch one => recovery fence violation.
    # M3 legacy claim disabled => abandoned legacy Writing never aborts.
    # W1-W3 start from the same Init's legacy partition and require reachable
    # takeover/repeated takeover/abort, excluding an empty legacy domain.
    source = (ROOT / "formal/tla/ManagedCredentialCreation.tla").read_text()
    config = (ROOT / "formal/tla/ManagedCredentialCreation.cfg").read_text()
    removal_config = config.replace("INVARIANT CreationSafety", "INVARIANTS AbortedCreationIsClean CreationSafety")
    guard = 'CompleteAbortCleanup ==\n    /\\ processUp /\\ phase = "ReclaimingAbort"\n    /\\ material = "None"'
    epoch = "ownerEpoch' = IF ownerEpoch = 0 THEN 2 ELSE ownerEpoch + 1"
    admission = r"/\ ownerEpoch < MaxOwnerEpoch"
    for anchor in (guard, epoch, admission):
        if source.count(anchor) != 1:
            raise SystemExit("Credential creation mutation anchor drifted; review the model")
    mutations = [
        (source.replace(guard, guard.rsplit("\n", 1)[0]), removal_config, "AbortedCreationIsClean"),
        (source.replace(epoch, "ownerEpoch' = ownerEpoch + 1"),
         config.replace("INVARIANT CreationSafety", "INVARIANTS RecoveryClaimChangesTheFence CreationSafety"),
         "RecoveryClaimChangesTheFence"),
        (source.replace(admission, r"/\ ownerEpoch > 0 /\ ownerEpoch < MaxOwnerEpoch"),
         config.replace("PROPERTY ", "PROPERTIES "), "temporal:AbandonedWritingEventuallyAborts"),
    ]
    # W4 also preserves the original fresh-command publication path.
    for initial, witness in (
        ("LegacyInit", "NoRecoveryReached"), ("LegacyInit", "NoAbortReached"),
        ("LegacyInit", "NoSuccessiveRecoveryReached"), ("FreshInit", "NoPublicationReached"),
    ):
        witness_config = f"INIT {initial}\nNEXT Next\nCONSTANT MaxOwnerEpoch = 3\nCHECK_DEADLOCK FALSE\n"
        mutations.append((source, witness_config + f"INVARIANT {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "ManagedCredentialCreation", source, config,
                             mutations, heap_mb=1024, timeout_seconds=120)
    print("Credential creation: 3/3 semantic mutants killed; legacy takeover/repeated takeover/abort and fresh publication witnesses reachable")


def check_work_queue_stop_model_mutants(tlc_jar: Path) -> None:
    # S1 exact request -> addressed authority clears; S2 wrong owner and S3
    # wrong epoch -> no Stop effect. General queue Safety cannot qualify these
    # guards, so both mutants must fail the precise request action property.
    source = (ROOT / "formal/tla/WorkQueueKernel.tla").read_text()
    config = (ROOT / "formal/tla/WorkQueue.cfg").read_text()
    beginning = "Stop(worker, item, expectedEpoch) ==\n"
    ending = "\\* Request/effect oracle for the existing Stop action"
    if source.count(beginning) != 1 or source.count(ending) != 1:
        raise SystemExit("WorkQueue Stop mutation boundary drifted")
    start = source.index(beginning)
    stop = source[start:source.index(ending, start)]
    # Isolate the request action oracle: otherwise the owner mutant first
    # trips StoppedHasNoStopEffect and never qualifies request binding itself.
    # The full unchanged CFG is still checked by strict CI and the witness run.
    guard_config = config.split("INVARIANTS\n", 1)[0] + "PROPERTIES StopRequestsAreBound\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for guard in (r"    /\ kOwner[item] = worker", r"    /\ kEpoch[item] = expectedEpoch"):
        if stop.count(guard) != 1:
            raise SystemExit("WorkQueue Stop request guard drifted")
        mutated = source.replace(stop, stop.replace(guard, r"    /\ TRUE", 1), 1)
        mutations.append((mutated, guard_config, "action:StopRequestsAreBound"))
    check_tlc_model_mutations(tlc_jar, "WorkQueueKernel", source, guard_config, mutations)
    # S4 nonvacuity: a legal Stop must reach Stopped, where the unchanged base
    # checks both disabled repeated Stop and ENABLED UNCHANGED kVars. Spec's
    # unlabeled stutter has no request or bool result; it does not prove the
    # stores' successful Stopped/same-epoch replay response.
    boundary = "============================================================================="
    if source.count(boundary) != 1:
        raise SystemExit("WorkQueue kernel module boundary drifted")
    witness = source.replace(boundary, r"NoStoppedWorkReached == \A item \in WorkItems: kState[item] # " + '"Stopped"\n' + boundary)
    check_tlc_model_mutations(
        tlc_jar, "WorkQueueKernel", witness, config,
        [(witness, config + "\nINVARIANT NoStoppedWorkReached\n", "NoStoppedWorkReached")],
    )
    print("WorkQueue Stop: 2/2 exact-request guard mutants killed; legal Stop and unlabeled Stopped stutter qualified")


def check_session_ownership_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect rules: O1 missing owner and O2 zero generation invalidate a
    # visible root; R1 false-empty and R2 mixed-owner reads violate exact capture
    # even when their row types are valid. The read pulse prevents false-empty
    # returns from hiding as stutters. Reachability witnesses include a live
    # read, a conflicting create, and a retained pre-tombstone snapshot.
    source = (ROOT / "formal/tla/SessionOwnership.tla").read_text()
    config = (ROOT / "formal/tla/SessionOwnership.cfg").read_text()
    header = config.split("INVARIANTS", 1)[0]
    mutations = []
    cases = [
        ("owner |-> kOwner", "owner |-> NoOwner", "OwnershipSafety"),
        ("generation |-> kRevision", "generation |-> 0", "OwnershipSafety"),
        ("Read == /\\ readSnapshot' = RootRow",
         "Read == /\\ readSnapshot' = NoRow", "action:ReadCapturesOneRootFact"),
        ("Read == /\\ readSnapshot' = RootRow",
         'Read == /\\ readSnapshot\' = IF kExistence = "Live" '
         'THEN [RootRow EXCEPT !.owner = CHOOSE o \\in Owners: o # kOwner] '
         'ELSE RootRow', "action:ReadCapturesOneRootFact"),
    ]
    for anchor, broken, invariant in cases:
        if source.count(anchor) != 1:
            raise SystemExit("Session ownership mutation anchor drifted")
        check = ("PROPERTIES " + invariant.removeprefix("action:")
                 if invariant.startswith("action:") else "INVARIANTS " + invariant)
        mutations.append((source.replace(anchor, broken),
                          header + check + "\nCHECK_DEADLOCK FALSE\n", invariant))
    # O3 reinstates the deleted writer's ability to rebind an already live
    # root. All other tuple coordinates remain equal; the exact action property
    # must fail, rather than a parser error or a malformed root type.
    anchor = "OwnershipNext == (Next /\\ UNCHANGED <<readSnapshot, readCompleted>>) \\/ Read"
    kernel = (ROOT / "formal/tla/SessionRootKernel.tla").read_text()
    coordinates = kernel.split("kVars == <<", 1)[1].split(">>", 1)[0].split(",")
    frame = ", ".join(name.strip() for name in coordinates if name.strip() != "kOwner")
    broken = anchor + (
        '\n    \\/ (/\\ kExistence = "Live"'
        '\n        /\\ kOwner\' = CHOOSE o \\in Owners: o # kOwner'
        f'\n        /\\ UNCHANGED <<{frame}>>'
        '\n        /\\ UNCHANGED <<readSnapshot, readCompleted>>)'
    )
    if source.count(anchor) != 1:
        raise SystemExit("Session ownership writer mutation anchor drifted")
    mutations.append((source.replace(anchor, broken),
                      header + "PROPERTIES OwnerNeverRebinds\n",
                      "action:OwnerNeverRebinds"))
    for witness in ("NoOwnedReadReached", "NoHistoricalReadAfterTombstone",
                    "NoConflictingReadReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "SessionOwnership", source, config,
                             mutations, dependencies=("SessionRootKernel",))
    print("Session ownership: 5/5 semantic mutants killed; 3/3 read witnesses reached")


def check_session_budget_admission_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect table mirrors the actual gated SQLite rows without replacing
    # them: A accepted active/settled wins; U raised-only stays Pending; R a
    # negative survives raise; E failed/unknown writes stay Pending; C a late
    # candidate loses its CAS; H either mismatched payload is never absence.
    # Root/receipt reads, candidate reduction, CAS and reply are separate. All
    # ten mutations retain both original kernels and the complete CFG. Positive
    # inverse invariants reject always-deny and impossible joint-action models.
    source = (ROOT / "formal/tla/SessionRunBudgetAdmission.tla").read_text()
    config = (ROOT / "formal/tla/SessionRunBudgetAdmission.cfg").read_text()

    def changed(before: str, after: str) -> str:
        if source.count(before) != 1:
            raise SystemExit(f"Budget admission mutation anchor drifted: {before}")
        return source.replace(before, after, 1)

    def oracle_config(oracle: str, cfg: str = config) -> str:
        # The unmodified full CFG is the green base. Isolate each negative
        # control's named oracle so another Safety conjunct cannot receive its
        # credit. No domain, kernel, primary assertion or tool budget changes.
        selected = "\n".join(line for line in cfg.splitlines()
                             if not line.startswith(("INVARIANTS ", "PROPERTIES ")))
        return selected + (f"\nPROPERTIES {oracle.removeprefix('action:')}\n"
                           if oracle.startswith("action:") else f"\nINVARIANTS {oracle}\n")

    def witness_config(witness: str, cfg: str = config) -> str:
        return cfg.replace("INVARIANTS Safety", f"INVARIANTS {witness} Safety")

    late_read = '!.phase = "ObservePositive", !.route = "Late",'
    late_decision = 'LateDecide(caller) ==\n    /\\ calls[caller].phase = "LateDecide"\n    /\\ IF calls[caller].readBudget'
    cas_result = '    /\\ CAS!Write(caller)\n    /\\ IF lastApplied\''
    lost_ack = '!.outcome = "Pending", !.cause = "LostAck",'
    outage = 'Finish(caller, "Pending", "Outage",'
    positive_exact = '[] calls[caller].positive = "Exact" ->'
    negative_exact = '[] calls[caller].negative = "Exact" ->'
    epoch = 'Activity!OpenAt(calls[caller].operation,\n                                        generation\' + InitialRootRevision)'
    raised = 'Finish(caller, "Pending", "Raised", {"RaisedPending"})'
    raise_frame = '    /\\ policyPhase\' = "Done"\n    /\\ UNCHANGED <<activityVars, negativeReceipts, calls,'
    changes = [
        (positive_exact, positive_exact.replace(' = "Exact" ->', ' = "Exact" /\\ calls[caller].seenEpoch \\in activeEpochs ->'), "AcceptedObservationIsNotDiscarded"),
        (negative_exact, '[] FALSE ->', "NoAcceptedNegativePair"),
        (late_read, '!.phase = "LateDecide", !.route = "Late",', "HealthyClosedReadReplaysAccepted"),
        (late_decision, late_decision.replace('IF calls[caller].readBudget', 'IF FALSE'), "action:NewRefusalWasBudgetRejected"),
        (cas_result, cas_result.replace("IF lastApplied'", "IF TRUE"), "action:RejectedCASPreservesDurableState"),
        (epoch, epoch.replace("generation' + InitialRootRevision)", "generation' + InitialRootRevision + 1)"), "action:ActivityCommitUsesExactRootRevision"),
        (lost_ack, lost_ack.replace('"Pending"', '"Rejected"'), "UncertaintyCannotReportRefusal"),
        (outage, outage.replace('"Pending"', '"Rejected"'), "UncertaintyCannotReportRefusal"),
        (raised, raised.replace('"Pending"', '"Rejected"'), "RaisedDecisionCannotCloseRefusal"),
        (raise_frame, raise_frame.replace('    /\\ UNCHANGED <<activityVars, negativeReceipts, calls,', '    /\\ negativeReceipts\' = {}\n    /\\ UNCHANGED <<activityVars, calls,'), "action:NegativeReceiptIsAbsorbing"),
    ]
    mutations = [(changed(before, after), oracle_config(oracle), oracle) for before, after, oracle in changes]
    witnesses = ("NoOpenCommit", "NoSettleCommit", "NoSettledReplay", "NoAcceptedAfterStaleBudget",
                 "NoRaisedPending", "NoNegativeAfterRaise", "NoNegativeCASLossToRaise",
                 "NoNegativeCASLossToAccepted", "NoBeforeWriteOutage", "NoNegativeLostAckRecovery",
                 "NoPositiveLostAckRecovery", "NoFastOpenCommit", "NoWorkerOpenCommit",
                 "NoFastNegativeCommit", "NoWorkerNegativeCommit", "NoPositiveStoreReplay", "NoNegativeStoreReplay")
    mutations.extend((source, witness_config(witness), witness) for witness in witnesses)
    dependencies = ("SessionActivityKernel", "AggregateCAS")
    # Resolve every anchor before invoking the runner, including H1/H2. A drift
    # fails source preparation rather than starting a partially selected suite.
    hash_cases = []
    for kind, field in (("Positive", "positive"), ("Negative", "negative")):
        before = f'calls[caller].{field} = "Mismatch" ->'
        mutant = changed(before, 'FALSE ->')
        cfg = config.replace('ReceiptFault = "None"', f'ReceiptFault = "{kind}"')
        witness = f"No{kind}HashRefusal"
        hash_cases.append((cfg, [(mutant, oracle_config("CorruptReceiptNeverBecomesAbsence", cfg), "CorruptReceiptNeverBecomesAbsence"),
                                 (source, witness_config(witness, cfg), witness)]))
    check_tlc_model_mutations(tlc_jar, "SessionRunBudgetAdmission", source, config, mutations, dependencies=dependencies)
    for cfg, cases in hash_cases:
        check_tlc_model_mutations(tlc_jar, "SessionRunBudgetAdmission", source, cfg, cases, dependencies=dependencies)
    print("Budget admission: semantic mutations and Open/Settle/read/replay reachability checked; finite abstract I/O ordering only")


if __name__ == "__main__":
    main()
