#!/usr/bin/env python3
"""Mutation-test the formal evidence and residual-boundary gates.

Metadata mutants prove that the repository gate also
rejects a proof removed from CI, a downgraded proof claim, and incomplete or
invented residual-boundary documentation. The opt-in Memory copy mutant uses
the pinned TLC supplied by the strict gate to check the recovery-base invariant.
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

from formal_gate_mutations import must_fail, run_gate_mutations, write_json
from formal_model_mutation_runner import check_tlc_model_mutations
from formal_config_mutations import check_config_cas_model_mutant, check_config_activation_model_mutants
from formal_worker_registration_mutations import check_worker_registration_model_mutants
from formal_worker_retirement_mutations import check_retained_worker_retirement_model_mutants
from formal_realization_capacity_mutations import check_realization_capacity_observation_mutants
from formal_realization_trace_mutations import check_realization_trace_mutants
from formal_event_batch_mutations import check_event_batch_model_mutants, check_event_batch_observation_mutants
from formal_erasure_mutations import check_erasure_checkpoint_model_mutants
from formal_eval_mutations import check_eval_evidence_model_mutants
from formal_delegation_mutations import check_delegation_model_mutants
from formal_session_deletion_mutations import (check_session_deletion_model_mutants, check_deletion_observation_mutants)
from formal_local_process_mutations import (
    check_local_process_model_mutants, check_local_recovery_model_mutants,
)
from formal_memory_mutations import (
    check_memory_extraction_model_mutants,
    check_memory_invalidation_model_mutants,
    check_memory_participant_model_mutants,
)
from formal_execution_replacement_mutations import (
    check_execution_replacement_model_mutants, check_k8s_process_observation_model_mutants,
    check_remote_attempt_model_mutants, check_session_continuation_model_mutants,
)
from formal_credential_mutations import (
    check_mcp_credential_model_mutants, check_credential_effect_model_mutants,
    check_acp_binding_model_mutants,
    check_acp_observation_model_mutants,
    check_acp_projection_model_mutants,
)
from formal_dispatch_mutations import (check_resolution_failure_model_mutant, check_terminal_scan_model_mutants,
    check_maintenance_observation_mutants, check_managed_projection_model_mutants)
from formal_background_mutations import check_background_completion_model_mutant
from formal_webhook_mutations import check_webhook_subscription_model_mutants


ROOT = Path(__file__).resolve().parents[2]


class MutationOracleTests(unittest.TestCase):
    def test_model_cli_flags_keep_their_exact_single_handler(self) -> None:
        # R1 each existing flag => exactly its handler, once, with parsed Path.
        # This compatibility list is independent of the production registry;
        # a lost registration or mismatched dispatch must fail before real TLC.
        flags = (
            "memory-copy-model-mutant", "agent-input-model-mutant",
            "resource-preparation-model-mutant", "realization-renewal-model-mutant",
            "resolution-failure-model-mutant", "terminal-scan-model-mutants", "config-cas-model-mutant",
            "background-completion-model-mutant", "sandbox-control-model-mutants",
            "audit-commit-model-mutants", "effect-retirement-model-mutant", "config-activation-model-mutants",
            "credential-creation-model-mutants", "verified-admission-model-mutants",
            "session-network-model-mutant", "work-queue-stop-model-mutants",
            "managed-projection-model-mutants", "session-observation-model-mutants",
            "cowork-lifecycle-model-mutants", "local-process-model-mutant",
            "local-recovery-model-mutants", "checkpoint-recovery-model-mutants",
            "mcp-credential-model-mutants", "credential-effect-model-mutants",
            "acp-binding-model-mutants",
            "acp-observation-model-mutants",
            "eval-evidence-model-mutants",
            "webhook-subscription-model-mutants",
            "k8s-process-observation-model-mutants",
            "retained-worker-retirement-model-mutants",
        )
        for flag in flags:
            handler = "check_" + flag.replace("-", "_")
            if flag == "local-process-model-mutant": handler += "s"
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

    def test_k8s_process_observation_keeps_one_complete_qualified_baseline(self) -> None:
        import formal_execution_replacement_mutations as family
        # KPT cause/effect rules: R1 canonical model+complete CFG, unique mutation
        # anchors and qualified 4096MiB/eight-worker/300s profile => one shared
        # runner receives the full green base, 16 unsafe variants and 6 reachable
        # observation/stop witnesses. R2 a missing anchor => fail before any TLC.
        # Constraint: all cases retain three generations and both participants;
        # the existing TLC oracle owns green-before-kill, exact exit/name and
        # timeout rejection (covered by the adjacent oracle decision tables).
        source = (ROOT / "formal/tla/K8sProcessObservation.tla").read_text()
        config = (ROOT / "formal/tla/K8sProcessObservation.cfg").read_text()
        jar = Path("tlc.jar")
        with patch.object(family, "check_tlc_model_mutations") as runner:
            family.check_k8s_process_observation_model_mutants(jar)
        runner.assert_called_once()
        args, budget = runner.call_args
        self.assertEqual(args[:4], (jar, "K8sProcessObservation", source, config))
        self.assertEqual(budget, {"heap_mb": 4096, "timeout_seconds": 300, "workers": 8})
        self.assertIn("MaxGeneration = 3", config)
        self.assertIn("Containers = {main, projector}", config)
        cases = args[4]
        self.assertEqual(len(cases), 22)
        for mutant, cfg, oracle in cases[:16]:
            self.assertNotEqual(mutant, source)
            self.assertEqual((cfg, oracle), (config, "Safety"))
        witnesses = ("NoObservedExit", "NoReplacementBinding", "NoReplacementExit",
                     "NoLiveStopRequest", "NoReplacementStop", "NoPostExitStopReceipt")
        self.assertEqual(cases[16:], [
            (source, config.replace("INVARIANT Safety", f"INVARIANT {name}"), name)
            for name in witnesses
        ])
        with patch.object(Path, "read_text", side_effect=[
            source.replace("g = current", "TRUE"), config,
        ]), patch.object(family, "check_tlc_model_mutations") as runner:
            with self.assertRaisesRegex(SystemExit, "mutation anchor drifted"):
                family.check_k8s_process_observation_model_mutants(jar)
            runner.assert_not_called()

    def test_constant_trace_refutation_requires_exact_name_and_status(self) -> None:
        # Q1 TLC rejects a false constant invariant before state exploration:
        # exact 151+named false relation is a kill. Q2 unrelated syntax/tool
        # failures, another relation or a successful exit are never kills.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in [(151, "The invariant of TraceRefines is equal to FALSE", True),
            (151, "The invariant of Other is equal to FALSE", False), (151, "parse error", False),
            (0, "The invariant of TraceRefines is equal to FALSE", False)]:
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[green, subprocess.CompletedProcess([], code, output)]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config", [("mutant", "config", "constant:TraceRefines")])
                else:
                    with self.assertRaises(SystemExit):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config", [("mutant", "config", "constant:TraceRefines")])

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
        # B4 the existing CLI handler reaches this same family before its old
        # families, with the exact parsed jar; no second registration is needed.
        with patch(f"{__name__}.check_session_budget_admission_model_mutants", side_effect=RuntimeError("budget family selected")) as selected:
            with self.assertRaisesRegex(RuntimeError, "budget family selected"):
                check_session_observation_model_mutants(Path("tlc.jar"))
            selected.assert_called_once_with(Path("tlc.jar"))
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
        "worker-registration-model-mutants": check_worker_registration_model_mutants,
        "retained-worker-retirement-model-mutants": check_retained_worker_retirement_model_mutants,
        "memory-copy-model-mutant": check_memory_copy_model_mutant,
        "agent-input-model-mutant": check_agent_input_model_mutant,
        "resource-preparation-model-mutant": check_resource_preparation_model_mutant,
        "file-reclamation-model-mutants": check_file_reclamation_model_mutants,
        "terminal-resource-model-mutants": check_terminal_resource_model_mutants,
        "event-batch-model-mutants": check_event_batch_model_mutants,
        "session-deletion-model-mutants": check_session_deletion_model_mutants,
        "remote-attempt-model-mutants": check_remote_attempt_model_mutants,
        "unmaterialized-cleanup-model-mutant": check_unmaterialized_cleanup_model_mutant,
        "realization-renewal-model-mutant": check_realization_renewal_model_mutant,
        "resolution-failure-model-mutant": check_resolution_failure_model_mutant,
        "terminal-scan-model-mutants": check_terminal_scan_model_mutants,
        "k8s-process-observation-model-mutants": check_k8s_process_observation_model_mutants,
        "config-cas-model-mutant": check_config_cas_model_mutant,
        "erasure-checkpoint-model-mutants": check_erasure_checkpoint_model_mutants,
        "config-activation-model-mutants": check_config_activation_model_mutants,
        "background-completion-model-mutant": check_background_completion_model_mutant,
        "sandbox-control-model-mutants": check_sandbox_control_model_mutants,
        "memory-invalidation-model-mutants": check_memory_invalidation_model_mutants,
        "memory-participant-model-mutants": check_memory_participant_model_mutants,
        "memory-extraction-model-mutants": check_memory_extraction_model_mutants,
        "audit-commit-model-mutants": check_audit_commit_model_mutants,
        "effect-retirement-model-mutant": check_effect_retirement_model_mutant,
        "credential-creation-model-mutants": check_credential_creation_model_mutants,
        "verified-admission-model-mutants": check_verified_admission_model_mutants,
        "session-network-model-mutant": check_session_network_model_mutant,
        "work-queue-stop-model-mutants": check_work_queue_stop_model_mutants,
        "session-observation-model-mutants": check_session_observation_model_mutants,
        "session-ownership-model-mutants": check_session_ownership_model_mutants,
        "session-start-root-model-mutants": check_session_start_root_model_mutants,
        "cowork-lifecycle-model-mutants": check_cowork_lifecycle_model_mutants,
        "local-process-model-mutant": check_local_process_model_mutants,
        "local-recovery-model-mutants": check_local_recovery_model_mutants,
        "checkpoint-recovery-model-mutants": check_checkpoint_recovery_model_mutants,
        "mcp-credential-model-mutants": check_mcp_credential_model_mutants,
        "credential-effect-model-mutants": check_credential_effect_model_mutants,
        "acp-binding-model-mutants": check_acp_binding_model_mutants,
        "acp-observation-model-mutants": check_acp_observation_model_mutants,
        "acp-projection-model-mutants": check_acp_projection_model_mutants,
        "managed-projection-model-mutants": check_managed_projection_model_mutants,
        "eval-evidence-model-mutants": check_eval_evidence_model_mutants,
        "webhook-subscription-model-mutants": check_webhook_subscription_model_mutants,
        "delegation-model-mutants": check_delegation_model_mutants,
    }
    model_selection = parser.add_mutually_exclusive_group()
    for name in model_checks:
        model_selection.add_argument(f"--{name}", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--session-observation-trace-mutants", type=Path, metavar="TLC_JAR")
    parser.add_argument("--observation-trace-dir", type=Path)
    arguments = parser.parse_args()
    if arguments.session_observation_trace_mutants is not None:
        if arguments.observation_trace_dir is None:
            parser.error("--observation-trace-dir is required")
        check_observation_trace_mutants(arguments.session_observation_trace_mutants, arguments.observation_trace_dir)
        return
    for name, check in model_checks.items():
        if (jar := getattr(arguments, name.replace("-", "_"))) is not None:
            check(jar)
            return
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(MutationOracleTests)
    if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
        raise SystemExit(1)
    run_gate_mutations()




def check_memory_copy_model_mutant(tlc_jar: Path) -> None:
    # Cause/effect rule C1: replace ONLY recovery's frozen expected head with
    # the current generation. E1: TLC must report the exact base-preservation
    # invariant, not merely a parse error, timeout or unrelated invariant.
    # The original positive configuration runs immediately before this in CI.
    # All mutation bytes are confined to a fresh temporary tree; no production
    # model is modified and there is no parallel CAS model implementation.
    source = (ROOT / "formal/tla/MemoryCAS.tla").read_text(encoding="utf-8")
    original = """CopyRecover(w) == /\\ crashed[w]
                  /\\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>"""
    mutant = """CopyRecover(w) == /\\ crashed[w]
                  /\\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\\ expected' = [expected EXCEPT ![w] = generation]
                  /\\ UNCHANGED <<generation, value, writes, sourcePresent, targetPresent,
                                  targetIdentity, renameCommitted, headPresent, lastAction,
                                  lastBefore, lastAfter, lastPresentBefore, lastPresentAfter,
                                  mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>"""
    if source.count(original) != 1:
        raise SystemExit("Memory copy recovery mutation anchor drifted; review the model")
    # C2: admit a request after an incomplete scan. The exact scan invariant must
    # reject the old missing-root-as-empty behavior; a tool failure cannot pass.
    scan_guard = "    /\\ scanComplete[w]"
    if source.count(scan_guard) != 1:
        raise SystemExit("Memory copy scan mutation anchor drifted; review the model")
    root_guard = "    /\\ rootMatches[w]"
    if source.count(root_guard) != 1:
        raise SystemExit("Memory copy root mutation anchor drifted; review the model")
    # L7: caller cancellation must not erase the request already executing in
    # SQLite. Check the action property, not a false claim that abort undoes SQL.
    crash_original = """CopyCrash(w) == /\\ mounted[w] /\\ ~crashed[w]
                /\\ crashed' = [crashed EXCEPT ![w] = TRUE]
                /\\ pendingAck' = [pendingAck EXCEPT ![w] = FALSE]
                /\\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, requestVars>>"""
    crash_mutant = crash_original.replace(
        "/\\ pendingAck'", """/\\ inFlight' = [inFlight EXCEPT ![w] = "none"]
                /\\ pendingAck'"""
    ).replace("lastMutationRoot, requestVars>>", "lastMutationRoot, requestScan, requestRoot>>")
    if source.count(crash_original) != 1:
        raise SystemExit("Memory copy cancellation mutation anchor drifted; review the model")
    mutations = [
        (source.replace(original, mutant), "CopyRecoveryKeepsMaterializationBase"),
        (source.replace(scan_guard, "    /\\ TRUE"),
         "CopyMutationRequiresCompleteScan"),
        (source.replace(root_guard, "    /\\ TRUE"),
         "CopyMutationRequiresOriginalRoot"),
    ]
    config = (ROOT / "formal/tla/MemoryCASCopy.cfg").read_text(encoding="utf-8")
    # MP: removing the commit-time path precondition reintroduces a write after
    # rename even when id/hash are unchanged. The named action property must
    # fail; a parsing error or unrelated failure is never a mutation kill.
    location_guard = "IF ~sourcePresent THEN CopyLocationConflict\n                   ELSE IF"
    if source.count(location_guard) != 1:
        raise SystemExit("Memory copy location mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "MemoryCAS", source, config,
                              [(mutated, config, invariant) for mutated, invariant in mutations]
                              + [(source.replace(crash_original, crash_mutant), config,
                                  "action:CopyCrashKeepsInFlight"),
                                  (source.replace(location_guard, "IF"), config,
                                  "action:CopyUpdateRequiresOriginalLocation")])
    print("Memory copy mutation score: 5/5 killed (frozen base, complete scan, original root, in-flight cancellation, original location)")
    # RC-M: fail the exact invariant/action, not just any nonzero TLC exit.
    retention_config = (ROOT / "formal/tla/MemoryCASRetention.cfg").read_text(encoding="utf-8")
    retention_mutants = []
    for original, replacement, invariant in [
        (r"/\ w \in retSaved /\ w \notin retCancelled",
         r"/\ w \in retSeen /\ w \notin retCancelled",
         "RetAcknowledgementRequiresDurability"),
        (r"/\ w \in retAck /\ w \notin retCancelled",
         r"/\ w \in retSeen /\ w \notin retCancelled",
         "RetDisposalRequiresDurability"),
        (r"/\ UNCHANGED <<copyVars, retSeen, retInFlight, retSaved, retDisposed>>",
         r"/\ retInFlight' = retInFlight \ {w}" + "\n"
         + r"    /\ UNCHANGED <<copyVars, retSeen, retSaved, retDisposed>>",
         "action:RetCancellationPreservesRequest"),
    ]:
        if source.count(original) != 1:
            raise SystemExit(f"Memory retention mutation anchor drifted: {invariant}")
        retention_mutants.append((source.replace(original, replacement), retention_config, invariant))
    check_tlc_model_mutations(tlc_jar, "MemoryCAS", source, retention_config,
                              retention_mutants)
    print("Memory retention mutation score: 3/3 killed (durable ack, disposal, late commit)")


def check_agent_input_model_mutant(tlc_jar: Path) -> None:
    # C1 lock omitted before reading absence -> E1 duplicate/stale commit.
    # Require a green original model, then the exact successor invariant failure;
    # parse failures, timeouts and unrelated errors are never mutation kills.
    source = (ROOT / "formal/tla/AgentInputRevision.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/AgentInputRevision.cfg").read_text(encoding="utf-8")
    anchor = "CONSTANT SerializeAdmissions = TRUE"
    if config.count(anchor) != 1:
        raise SystemExit("Agent input serialization mutation anchor drifted; review the model")
    mutant = config.replace(anchor, "CONSTANT SerializeAdmissions = FALSE")
    check_tlc_model_mutations(tlc_jar, "AgentInputRevision", source, config,
                              [(source, mutant, "AppliedIsExactSuccessor")])
    print("Agent input mutation score: 1/1 killed (missing-row admission lock)")


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


def check_realization_publication_model_mutants(tlc_jar: Path) -> None:
    # R1 all Stage receipts republished after the activation switch; R2 exact
    # retry forgets Draining receipts. Each negative control must violate its
    # named safety invariant, never count a parser failure/timeout as a kill.
    # Two witnesses prove recovery and response loss are actually reachable.
    source = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationPublication.cfg").read_text()
    header = config.split("INVARIANTS", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ("publish |-> PublicationCandidates(@.staged, ActiveGenerations)", "publish |-> @.staged",
         "PublicationExcludesRetiredGenerations"),
        ("(publication[session].publish \\cup publication[session].drain)",
         "publication[session].publish", "LostActivationCanReplay"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Realization publication mutation anchor drifted; review the model")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoRecoveredPublicationReached", "NoLostActivationReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", source, config, mutations)
    print("Realization publication: 2/2 mutants killed; recovery/response-loss witnesses reachable")


def check_terminal_resource_model_mutants(tlc_jar: Path) -> None:
    # T1 restores the actual active-only projection bug. T2 admits a cleanup
    # receipt before installing its complete command. Require the exact safety
    # counterexample, never a parse failure, tool outage, or liveness timeout.
    source = (ROOT / "formal/tla/SessionResourceActivation.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/SessionResourceActivation.cfg").read_text(encoding="utf-8")
    projection = "                  /\\ cleanupRevision' = DesiredRevision"
    receipt = "           /\\ cleanupProjected /\\ cleanupRevision = DesiredRevision"
    if source.count(projection) != 1 or source.count(receipt) != 1:
        raise SystemExit("Terminal Resource mutation anchor drifted; review the model")
    mutations = [
        (source.replace(projection, projection.replace("DesiredRevision", "activeRevision")),
         config, "CleanupProjectionCoversDesired"),
        (source.replace(receipt, "           /\\ TRUE"),
         config, "CleanupRequiresCompleteProjection"),
    ]
    check_tlc_model_mutations(tlc_jar, "SessionResourceActivation", source, config, mutations)
    print("Terminal Resource mutation score: 2/2 killed (active-only projection, premature receipt)")


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


def check_unmaterialized_cleanup_model_mutant(tlc_jar: Path) -> None:
    source = (ROOT / "formal/tla/SessionUnmaterializedCleanup.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/SessionUnmaterializedCleanupTarget.cfg").read_text(encoding="utf-8")
    anchor = "            /\\ joined\n"
    if source.count(anchor) != 1:
        raise SystemExit("Unmaterialized cleanup mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "SessionUnmaterializedCleanup", source, config,
        [(source.replace(anchor, "            /\\ TRUE\n"), config, "NoUndrainedCompletion")])
    print("Unmaterialized cleanup mutation score: 1/1 killed (absence without physical join)")


def check_realization_renewal_model_mutant(tlc_jar: Path) -> None:
    check_realization_publication_model_mutants(tlc_jar)
    # S1-S9 extend the existing Worker lifecycle family: startup and withdrawal
    # share the same registration owner as drain, not a second cleanup model.
    source = (ROOT / "formal/tla/WorkerDrain.tla").read_text()
    config = (ROOT / "formal/tla/WorkerDrain.cfg").read_text()
    header = "SPECIFICATION Spec\nCONSTANT MaxClaims = 3\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, broken, invariant in [
        ('AcceptProtocol == /\\ startupPhase = "checking_protocol"\n                  /\\ protocolCompatible',
         'AcceptProtocol == /\\ startupPhase = "checking_protocol"', "RegistrationEvidenceRequired"),
        ('/\\ ttlPositive\n                  /\\ startupPhase',
         '/\\ startupPhase', "RegistrationEvidenceRequired"),
        ('RejectProtocol == /\\ startupPhase = "checking_protocol"\n                  /\\ ~(protocolCompatible /\\ ttlPositive)',
         'RejectProtocol == /\\ FALSE', "temporal:InvalidRegistrationTerminates"),
        ('AcceptInitialReady == /\\ startupPhase = "awaiting"',
         'AcceptInitialReady == /\\ startupPhase = "probing"', "StartupAdmissionRequiresProbe"),
        ('BeginWithdrawal == /\\ withdrawal = "none"',
         'BeginWithdrawal == /\\ withdrawalCalls < 2', "OneWithdrawal"),
        ('(IF outcome = "applied" THEN "Dead" ELSE registryState)',
         '"Dead"', "RemoteDeathRequiresReceipt"),
        ('(startupPhase = "failed" \\/ shutdown.cleanupFinished)',
         'shutdown.cleanupFinished', "temporal:StartupFailureTerminates"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Worker startup/withdrawal mutation anchor drifted")
        property_line = ("PROPERTIES " + invariant.removeprefix("temporal:")
                         if invariant.startswith("temporal:") else "INVARIANTS " + invariant)
        mutations.append((source.replace(anchor, broken), header + property_line, invariant))
    for witness in ("NoFailedReturnReached", "NoReadyClaimReached",
                    "NoUnknownWithdrawalReached", "NoPendingHeartbeatCancelled",
                    "NoIncompatibleReturnReached", "NoZeroTtlReturnReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "WorkerDrain", source, config, mutations)
    print("Worker startup/withdrawal: 7/7 mutants killed; six lifecycle witnesses reachable")
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
    check_session_continuation_model_mutants(tlc_jar)
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
    check_session_execution_binding_model_mutants(tlc_jar)
    check_execution_replacement_model_mutants(tlc_jar)


def check_session_execution_binding_model_mutants(tlc_jar: Path) -> None:
    # S1-S21: independently remove each authority/identity guard; release the
    # original mutex between check and effect; permit a late weak upgrade or
    # rebinding. The receipt oracle is independent of the mutated predicate.
    source = (ROOT / "formal/tla/SessionExecutionBinding.tla").read_text()
    config = (ROOT / "formal/tla/SessionExecutionBinding.cfg").read_text()
    header = "SPECIFICATION BindingSpec\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, broken, invariant in [
        ('CurrentBinding == /\\ ExecutionRootCurrent', 'CurrentBinding == /\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ projection = "Live" /\\ ~sessionExpired', '/\\ TRUE /\\ ~sessionExpired', "OnlyCurrentBindingCommits"),
        ('/\\ ~sessionExpired\n  /\\ boundSlot', '/\\ TRUE\n  /\\ boundSlot', "OnlyCurrentBindingCommits"),
        ('/\\ boundSlot = slotIdentity', '/\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ boundGeneration = lease[1]', '/\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ boundEnvironment = environmentIdentity', '/\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ environmentPhase \\in {"candidate", "resident"}))\n\nSelect', '/\\ TRUE))\n\nSelect', "OnlyCurrentBindingCommits"),
        ('/\\ ~effectHeld /\\ UNCHANGED bindingVars', '/\\ UNCHANGED bindingVars', "OnlyCurrentBindingCommits"),
        ('Select == /\\ selection = "unset"', 'Select == /\\ selection \\in {"unset", "weak"}', "WeakExecutionCannotUpgrade"),
        ('/\\ boundSlot = 0 /\\ projection', '/\\ projection', "BindingIsWriteOnce"),
        ('/\\ deploymentReady /\\ selection\' = "scoped"', '/\\ selection\' = "scoped"', "OnlyProbedDeploymentSelected"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Session execution binding mutation anchor drifted: {anchor}")
        property_kind = "PROPERTY" if invariant == "BindingIsWriteOnce" else "INVARIANTS"
        mutations.append((source.replace(anchor, broken), header + property_kind + " " + invariant,
                          "action:" + invariant if property_kind == "PROPERTY" else invariant))
    # P1-P6 bypass each shared guard ONLY for a pre-Environment provider
    # callback. Execution-mode counterexamples cannot kill these mutants;
    # this prevents the new branch from borrowing the old branch's proof.
    for anchor, broken in [
        ('CurrentBinding == /\\ ExecutionRootCurrent', 'CurrentBinding == /\\ (providerSource \\/ ExecutionRootCurrent)'),
        ('/\\ projection = "Live" /\\ ~sessionExpired', '/\\ (providerSource \\/ projection = "Live") /\\ ~sessionExpired'),
        ('/\\ ~sessionExpired\n  /\\ boundSlot', '/\\ (providerSource \\/ ~sessionExpired)\n  /\\ boundSlot'),
        ('/\\ boundSlot = slotIdentity', '/\\ (providerSource \\/ boundSlot = slotIdentity)'),
        ('/\\ boundGeneration = lease[1]', '/\\ (providerSource \\/ boundGeneration = lease[1])'),
        ('/\\ ~effectHeld /\\ UNCHANGED bindingVars', '/\\ (providerSource \\/ ~effectHeld) /\\ UNCHANGED bindingVars'),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Provider fence source mutation anchor drifted: {anchor}")
        invariant = "OnlyCurrentBindingCommits"
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # M1-M3: a queued request cannot commit before acquiring the mount lock;
    # a cached Copy ack cannot skip current authority; replay cannot redrain.
    for anchor, broken, invariant in [
        ('/\\ memoryWait # 1\n', '/\\ TRUE\n', "OnlyCurrentBindingCommits"),
        ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ (memoryDrains = 1 \\/ CurrentBinding)', "OnlyCurrentBindingCommits"),
        ('IF memoryWait = 2 THEN 1 ELSE memoryDrains', 'IF memoryWait = 2 THEN memoryDrains + 1 ELSE memoryDrains', "MemoryCopyDrainedOnce"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Memory acknowledgement mutation anchor drifted: {anchor}")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # D1-D3: pending preparation cannot report an effect; a completed wait and
    # a cached Memory ack cannot bypass the shared current authority. The
    # independent receipt oracle is unchanged, including zero-Memory paths.
    for anchor, broken in [
        ('/\\ providerWait # 1\n', '/\\ TRUE\n'),
        ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ (providerWait = 2 \\/ CurrentBinding)'),
        ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ ((providerWait = 2 /\\ memoryDrains = 1) \\/ CurrentBinding)'),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Disposal preparation mutation anchor drifted: {anchor}")
        invariant = "OnlyCurrentBindingCommits"
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # One constructor for each phase-only pending-response,
    # post-response-authority and exact-retry mutant. Another phase cannot
    # supply its counterexample; retry requires an actual prior phase effect.
    # Compensation and backend cancellation remain adapter obligations.
    for phase, selector in (("disposal", "physicalDisposal"),
                            ("reconstruction", "coldReconstruction"),
                            ("restore-disposal", "restoredDisposal"),
                            ("checkpoint", "checkpointUpload"), ("adoption", "liveAdoption"),
                            ("creation", "localCreation")):
        for anchor, broken in [
            ('/\\ providerWait # 1\n', '/\\ (' + selector + ' \\/ providerWait # 1)\n'),
            ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ ((' + selector + ' /\\ providerWait = 2) \\/ CurrentBinding)'),
            ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ ((' + selector + ' /\\ "' + phase + '" \\in effectPhases) \\/ CurrentBinding)'),
        ]:
            if source.count(anchor) != 1:
                raise SystemExit(f"{phase} mutation anchor drifted: {anchor}")
            invariant = "OnlyCurrentBindingCommits"
            mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # CT1-CT6: fix a redundant Provider clock in its rejecting class, separately
    # for each ordinary phase (restored disposal shares disposal). The positive
    # oracle expands current-authority coordinates independently of the mutant;
    # a safety-only check could otherwise pass by denying every fresh effect.
    anchor = 'CheckEffect == /\\ ~effectHeld /\\ effects < 2 /\\ CurrentBinding'
    if source.count(anchor) != 1:
        raise SystemExit("Provider clock over-denial mutation anchor drifted")
    for phase in ("preparation", "disposal", "reconstruction", "checkpoint", "adoption", "creation"):
        broken = anchor + '\n  /\\ ~(providerSource /\\ providerPhase = "' + phase + '")'
        invariant = "FreshProviderEffectRemainsEnabled"
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # Both topology paths must reach actual effects. Otherwise an accidental
    # mandatory Worker guard could make all direct-local safety checks vacuous.
    for witness in ("NoCandidateEffectReached", "NoResidentEffectReached",
                    "NoLocalEffectReached", "NoRegisteredEffectReached",
                    "NoProviderEffectReached", "NoUnreadyProviderEffectReached",
                    "NoMemoryAckReached", "NoMemoryReplayReached",
                    "NoEmptyPreparationReached", "NoCachedPreparationReached",
                    "NoPhysicalDisposalReached", "NoCachedDisposalReached",
                    "NoColdReconstructionReached", "NoColdReconstructionReplayReached",
                    "NoRestoredDisposalReached", "NoRestoredDisposalReplayReached",
                    "NoCheckpointUploadReached", "NoCheckpointReplayReached",
                    "NoLiveAdoptionReached", "NoLiveAdoptionReplayReached",
                    "NoLocalCreationReached", "NoLocalCreationReplayReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "SessionExecutionBinding", source, config, mutations,
                              dependencies=("SessionRealizationAuthority", "WorkerExecutionAuthority"),
                              # Seven phase classes retain the same complete
                              # two-effect state space; raise tool resources,
                              # never truncate exploration to fit the old budget.
                              # The unchanged full baseline exceeded 600s on
                              # one TLC worker (not an observation timeout).
                              # Use four workers and a bounded 1800s budget;
                              # retain all states, invariants and mutants, and
                              # report metrics so capacity is not a silent gate.
                              heap_mb=4096, workers=4, timeout_seconds=1800,
                              report_metrics=True)
    print("Session execution binding: 47/47 mutants killed; 22 witnesses cover execution, provider projection, Memory, preparation, physical/restored disposal, cold reconstruction, live adoption, local creation and checkpoint upload/replay; fresh provider admission cannot be suppressed by a second clock")





def check_sandbox_control_model_mutants(tlc_jar: Path) -> None:
    # SC1 first close forgets participant; SC2 cancellation forgets it; SC3
    # return skips join; SC4 Drop releases early; SC5 late start after cancel;
    # SC6 join before exit; SC7 disable completion. Each uses the exact oracle
    # and green baseline; four witnesses exclude unreachable race/terminal paths.
    source = (ROOT / "formal/tla/SandboxControlPublication.tla").read_text()
    config = (ROOT / "formal/tla/SandboxControlPublication.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, oracle, kind in [
        ('retained\' = (phase = "Active")', "retained' = FALSE", "ReturnRequiresJoin", "INVARIANTS"),
        ('closers\' = [closers EXCEPT ![i] = "Cancelled"]\n    /\\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,\n                   task, cancelled, retained>>',
         'closers\' = [closers EXCEPT ![i] = "Cancelled"]\n    /\\ retained\' = FALSE\n    /\\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,\n                   task, cancelled>>',
         "ReturnRequiresJoin", "INVARIANTS"),
        ('/\\ (~retained \\/ task \\in Quiescent)', '/\\ TRUE', "ReturnRequiresJoin", "INVARIANTS"),
        ('/\\ task \\in Quiescent\n    /\\ phase\' = "Vacant"', '/\\ phase\' = "Vacant"',
         "ReleaseRequiresJoin", "INVARIANTS"),
        ('phase = "Active" /\\ ~cancelled /\\ task = "Absent"',
         'phase = "Active" /\\ task = "Absent"', "NoStartAfterCancellation", "PROPERTIES"),
        ('JoinTask ==\n    /\\ task = "Exited"', 'JoinTask ==\n    /\\ task \\in {"Running", "Exited"}',
         "JoinRequiresExit", "PROPERTIES"),
        ('FinishClose(i) ==\n    /\\ closers[i] = "Waiting"', 'FinishClose(i) ==\n    /\\ FALSE\n    /\\ closers[i] = "Waiting"',
         "temporal:DisposalEventuallyReturns", "PROPERTIES"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Sandbox control mutation anchor drifted: {oracle}")
        selected = oracle.removeprefix("temporal:")
        mutations.append((source.replace(anchor, broken), header + f"{kind} {selected}\n",
                          "action:" + oracle if kind == "PROPERTIES" and not oracle.startswith("temporal:") else oracle))
    for witness in ("NoConcurrentWaitReached", "NoCancelledPendingReached",
                    "NoFinishedDisposalReached", "NoDroppedPendingReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "SandboxControlPublication", source, config, mutations)
    print("Sandbox control mutation score: 7/7 killed; 4 disposal/drop schedules reachable")


def check_audit_commit_model_mutants(tlc_jar: Path) -> None:
    # A1 remove exact request equality -> wrong intent commits. A3 allow a
    # committed row through the pending guard -> duplicate business write.
    # Each kill requires the green baseline and the named semantic invariant.
    source = (ROOT / "formal/tla/AuditCommit.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/AuditCommit.cfg").read_text(encoding="utf-8")
    exact = r" /\ auditSummary[k] = summary"
    pending = 'auditState[k] = "pending"' + exact
    if source.count(exact) != 1 or source.count(pending) != 1:
        raise SystemExit("Audit commit mutation anchors drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "AuditCommit", source, config, [
        (source.replace(exact, ""), config, "ExactIntentOwnsBusinessWrite"),
        (source.replace(pending, 'auditState[k] # "none"' + exact),
         config, "BusinessCommitIffAuditCommitted"),
    ])
    print("Audit commit mutation score: 2/2 killed (exact intent, committed replay)")


def check_effect_retirement_model_mutant(tlc_jar: Path) -> None:
    # ABA rule: removing exact receipt/current identity matching lets a delayed
    # old worker delete the successor journal without applying its resource state.
    source = (ROOT / "formal/tla/ResourceBindingEffect.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/ResourceBindingEffect.cfg").read_text(encoding="utf-8")
    guard = "IF effectQueued = captured[w] THEN 0 ELSE effectQueued"
    if source.count(guard) != 1:
        raise SystemExit("Effect retirement mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "ResourceBindingEffect", source, config, [
        (source.replace(guard, "0"), config, "CommittedDraftHasAuditAndRecoverableBinding"),
    ])
    print("Effect retirement mutation score: 1/1 killed (stale receipt retires successor)")


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


def check_session_network_model_mutant(tlc_jar: Path) -> None:
    # N7: the old duplicated intersection admitted the trimmed empty identity.
    # Omitting canonicalization must violate the independent nonblank invariant;
    # parser errors or infrastructure failure are not semantic mutation kills.
    source = (ROOT / "formal/tla/SessionNetworkPolicy.tla").read_text()
    config = (ROOT / "formal/tla/SessionNetworkPolicy.cfg").read_text()
    guard = r"Meet(left, right) == Canonical(left) \intersect Canonical(right)"
    if source.count(guard) != 1:
        raise SystemExit("Session network mutation anchor drifted; review the model")
    mutant = source.replace(guard, r"Meet(left, right) == left \intersect right")
    check_tlc_model_mutations(tlc_jar, "SessionNetworkPolicy", source, config,
                             [(mutant, config, "NoBlankHostIsGranted")])
    # S1..S4 decision table: one canonical two-host source, all seven output
    # sequences of length 0..2, every position map and all four allowed sets.
    # The 84 cases independently check exact membership, canonical order and
    # output cardinality. Missing content binding/completeness/order/injectivity
    # must each yield a semantic counterexample after a green baseline.
    sequence_oracle = r"""
SequenceOutputCases == {<<>>, <<0>>, <<1>>, <<0, 0>>, <<0, 1>>, <<1, 0>>, <<1, 1>>}
SequenceExpected(allowed) ==
    IF allowed = {} THEN <<>> ELSE
    IF allowed = {0} THEN <<0>> ELSE
    IF allowed = {1} THEN <<1>> ELSE <<0, 1>>
SequenceProjectionOracle ==
    \A output \in SequenceOutputCases:
      \A selection \in [DOMAIN output -> 1..2], allowed \in SUBSET {0, 1}:
        LET bound == SelectedSequence(<<0, 1>>, output, selection)
            exact == ExactSelection(<<0, 1>>, output, selection, allowed)
        IN /\ (bound /\ exact => SequenceHosts(output) = allowed)
           /\ (bound /\ exact /\ IncreasingSelection(output, selection) =>
                 output = SequenceExpected(allowed))
           /\ (bound /\ exact /\ InjectiveSelection(output, selection) =>
                 Len(output) = Cardinality(allowed))
"""
    end = "============================================================================="
    if source.count(end) != 1:
        raise SystemExit("Session network module boundary drifted")
    sequence_source = source.replace(end, sequence_oracle + "\n" + end)
    sequence_config = config.replace("INVARIANTS ", "INVARIANTS SequenceProjectionOracle ", 1)
    anchors = [
        (r"/\ \A i \in DOMAIN output: output[i] = source[selection[i]]", r"/\ TRUE"),
        (r"(source[i] \in allowed) <=> (\E j \in DOMAIN output: selection[j] = i)", "TRUE"),
        (r"\A i, j \in DOMAIN output: i < j => selection[i] < selection[j]", "TRUE"),
        (r"\A i, j \in DOMAIN output: selection[i] = selection[j] => i = j", "TRUE"),
    ]
    sequence_mutants = []
    for anchor, replacement in anchors:
        if sequence_source.count(anchor) != 1:
            raise SystemExit("Session network sequence mutation anchor drifted")
        sequence_mutants.append((sequence_source.replace(anchor, replacement), sequence_config,
                                 "constant:SequenceProjectionOracle"))
    check_tlc_model_mutations(tlc_jar, "SessionNetworkPolicy", sequence_source,
                             sequence_config, sequence_mutants)
    print("Session network mutation score: 5/5 killed (blank and exact canonical sequence selection)")


def check_verified_admission_model_mutants(tlc_jar: Path) -> None:
    # R2/R3/R8: omit one target binding, expiry or release guard. Only the named safety
    # violation after a green baseline counts; tool errors never count as kills.
    source = (ROOT / "formal/tla/VerifiedProfileAdmission.tla").read_text()
    config = (ROOT / "formal/tla/VerifiedProfileAdmission.cfg").read_text()
    mutations = []
    for guard, invariant in (
        (r"              /\ \A d \in Dependencies : snapshot.versions[d] = 0", "AcceptedHasExactFingerprint"),
        (r"              /\ \A d \in Dependencies : snapshot.configurations[d] = 0", "AcceptedHasExactFingerprint"),
        (r"                    /\ snapshot.now - snapshot.observed[d] <= 1", "AcceptedHasFreshEvidence"),
        ("ReleaseAdmissible == /\\ release.artifactMatches", "ReleaseConsumptionHasExactExecutedEvidence"),
        (r"                     /\ release.profileMatches", "ReleaseConsumptionHasExactExecutedEvidence"),
        (r"                     /\ release.proofComplete", "ReleaseConsumptionHasExactExecutedEvidence"),
        (r"                     /\ release.sourceMatches", "ReleaseConsumptionHasExactExecutedEvidence"),
    ):
        if source.count(guard) != 1:
            raise SystemExit("Verified admission mutation anchor drifted; review the model")
        replacement = "ReleaseAdmissible == /\\ TRUE" if guard.startswith("ReleaseAdmissible ==") else ""
        mutations.append((source.replace(guard, replacement), config, invariant))
    image_guard = r'IF ~release.imageRequired \/ release.imageEvidence = "ExactSubject"'
    if source.count(image_guard) != 1:
        raise SystemExit("Image payload mutation anchor drifted; review the model")
    mutations.append((source.replace(image_guard, "IF TRUE"), config, "ImageConsumptionHasExactPayload"))
    # I3: a good payload checked under another image identity is not evidence
    # for the promoted subject. Omitting only that binding must expose it.
    mutations.append((source.replace(image_guard, r'IF ~release.imageRequired \/ release.imageEvidence # "Mismatch"'),
                      config, "ImageConsumptionBindsCheckedSubject"))
    check_tlc_model_mutations(tlc_jar, "VerifiedProfileAdmission", source, config, mutations)
    print("Verified admission mutation score: 9/9 killed (target freshness, exact executed release and bound image payload)")



def check_session_start_root_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect table: S1 acceptance without Create loses the durable root;
    # S2 Work row without Root.ProjectWork violates its sole authority; S3 a
    # terminal failure deleting its accepted root loses queryability; S4 early
    # Dispatch publication bypasses complete-root creation. Select each exact
    # semantic invariant separately, so another safety rule cannot mask a kill.
    # The four inverted positive witnesses establish all antecedents are live.
    source = (ROOT / "formal/tla/SessionStartProtocol.tla").read_text()
    config = (ROOT / "formal/tla/SessionStartProtocol.cfg").read_text()
    header = config.split("INVARIANTS", 1)[0]
    mutations = []
    for anchor, broken, invariant in (
        ('/\\ Root!Create(SessionOwner, Payload, "Worker", TRUE)',
         '/\\ UNCHANGED rootVars', 'AcceptedResponseIsDurableRoot'),
        ('/\\ Root!ProjectWork\n', '/\\ UNCHANGED rootVars\n',
         'WorkProjectionHasOneAuthority'),
    ):
        if source.count(anchor) != 1:
            raise SystemExit("Session start mutation anchor drifted")
        mutations.append((source.replace(anchor, broken),
                          header + f"INVARIANTS {invariant}\nCHECK_DEADLOCK FALSE\n", invariant))
    # These illegal writers each change exactly one canonical coordinate. The
    # remaining frame is derived from the actual component tuples, not copied.
    coordinates = []
    for name in ("rootVars", "eventVars", "workVars"):
        coordinates.extend(part.strip() for part in source.split(name + " == <<", 1)[1].split(">>", 1)[0].split(","))
    coordinates += ["workRowPresent", "realizationWorkEpoch", "acceptedResponse"]
    anchor = '    \\/ (UNCHANGED acceptedResponse /\\ OtherNext)'
    if source.count(anchor) != 1:
        raise SystemExit("Session start writer anchor drifted")
    for target, guard, value, invariant in (
        ('existence', 'acceptedResponse /\\ execution = "ActivationFailed"',
         '"Absent"', 'AcceptedFailureRemainsQueryable'),
        ('dispatchExists', 'existence = "Absent"',
         'TRUE', 'CompleteRootPrecedesEveryProjection'),
    ):
        frame = ", ".join(name for name in coordinates if name != target)
        action = f"\n    \\/ (/\\ {guard}\n        /\\ {target}' = {value}\n        /\\ UNCHANGED <<{frame}>>)"
        mutations.append((source.replace(anchor, anchor + action),
                          header + f"INVARIANTS {invariant}\nCHECK_DEADLOCK FALSE\n", invariant))
    for witness in ("NoAcceptedPreparingRoot", "NoAcceptedFailure", "NoWorkProjection", "NoDispatchProjection"):
        mutations.append((source, header + f"INVARIANTS {witness}\nCHECK_DEADLOCK FALSE\n", witness))
    check_tlc_model_mutations(tlc_jar, "SessionStartProtocol", source, config, mutations,
                             dependencies=("SessionRootKernel", "SessionEventKernel", "WorkQueueKernel",
                                           "RunIngressKernel", "SessionActivityKernel"))
    print("Session start root: 4/4 semantic mutants killed; 4/4 authority witnesses reached")


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


def check_session_observation_model_mutants(tlc_jar: Path) -> None:
    check_session_budget_admission_model_mutants(tlc_jar)
    # Decision-table mutation witnesses: M1 unchanged revision resamples time;
    # M2 processed replay resamples; M3 cold recovery invents legacy time;
    # M4 Stage's retired generation escapes into Publish. Every case must have
    # a green baseline and fail its exact invariant/property, not parse/timeout.
    root = (ROOT / "formal/tla/SessionRoot.tla").read_text()
    config = (ROOT / "formal/tla/SessionRoot.cfg").read_text().split("PROPERTIES")[0]
    config += "PROPERTIES NoRevisionChangePreservesTime\n"
    anchor = "    ELSE before\nRootObservationStep"
    assert root.count(anchor) == 1
    mutant = root.replace(anchor, "    ELSE IF before = UnknownTime THEN before ELSE IF now > before THEN now ELSE before\nRootObservationStep")
    check_tlc_model_mutations(tlc_jar, "SessionRoot", root, config,
        [(mutant, config, "action:NoRevisionChangePreservesTime")], dependencies=("SessionRootKernel",))

    event = (ROOT / "formal/tla/SessionEventProtocol.tla").read_text()
    config = (ROOT / "formal/tla/SessionEventProtocol.cfg").read_text().replace("PROPERTY First", "PROPERTIES First")
    anchor = "THEN now ELSE before"
    assert event.count(anchor) == 1
    mutant = event.replace(anchor, 'THEN now ELSE IF wasProcessed THEN now ELSE before')
    check_tlc_model_mutations(tlc_jar, "SessionEventProtocol", event, config,
        [(mutant, config, "action:FirstProcessingTimeIsImmutable")],
        dependencies=("SessionEventKernel", "SessionActivityKernel", "RunIngressKernel"))

    # I1 anchoring without an observed input invents committed truth; I2 an
    # input-only observation cannot release the Run activity. These mutations
    # exercise the same kernel consumed by the Event and Start compositions.
    kernel = (ROOT / "formal/tla/SessionEventKernel.tla").read_text()
    # The kernel defines none of the protocol's temporal or action properties,
    # so it inherits no property line at all; naming each one to drop broke
    # the moment the protocol config gained a new property.
    kernel_config = "\n".join(line for line in config.splitlines()
        if not line.lstrip().startswith(("PROPERTY ", "PROPERTIES "))
        and not any(word in line for word in ("ObservationTimes =", "UnknownTime =")))
    guard = 'AnchorCommittedInput ==\n    /\\ kEventPhase = "Activated"\n    /\\ kCommittedDisposition = "Running"'
    assert kernel.count(guard) == 1
    unproven = kernel.replace(guard, 'AnchorCommittedInput ==\n    /\\ kEventPhase = "Activated"')
    release = 'SettleRunActivity ==\n    /\\ kEventPhase = "Anchored"'
    assert kernel.count(release) == 1
    premature = kernel.replace(release, 'SettleRunActivity ==\n    /\\ kEventPhase \\in {"Activated", "Anchored"}')
    check_tlc_model_mutations(tlc_jar, "SessionEventKernel", kernel, kernel_config,
        [(unproven, kernel_config, "Safety"), (premature, kernel_config, "Safety")],
        dependencies=("SessionActivityKernel", "RunIngressKernel"))

    history = (ROOT / "formal/tla/StoreHistory.tla").read_text()
    config = (ROOT / "formal/tla/StoreHistory.cfg").read_text()
    # Small mutation witness only; the unchanged canonical CFG still runs the
    # complete two-identity, three-commit qualification in the strict gate.
    for old, new in [("{t1, t2}", "{t1}"), ("{r1, r2}", "{r1}"), ("{o1, o2}", "{o1}"), ("{p1, p2}", "{p1}")]:
        config = config.replace(old, new)
    anchor = "recoveredPrefix' = history"
    assert history.count(anchor) == 1
    mutant = history.replace(anchor, "recoveredPrefix' = [index \\in 1..Len(history) |-> [history[index] EXCEPT !.observedAt = IF @ = UnknownTime THEN 0 ELSE @]]")
    check_tlc_model_mutations(tlc_jar, "StoreHistory", history, config,
        [(mutant, config, "RecoveryPreservesExactCommittedMetadata")])

    # Publication partition and lost-response mutations belong to the same
    # driver's check_realization_publication_model_mutants, already invoked by
    # the strict renewal gate. Keep one owner instead of a second coarse graph.
    # S1-S3: a surviving resident Arc cannot bypass the physical observation.
    # Restore the old caller bypass in the existing owner model, then require
    # its named routing invariant to fail (not a parser/tool/timeout failure).
    projection = (ROOT / "formal/tla/SessionEnvironmentProjection.tla").read_text()
    config = (ROOT / "formal/tla/SessionEnvironmentProjection.cfg").read_text()
    for old, new in [("{effect_a, effect_b}", "{effect_a}"),
                     ("{generation_1, generation_2}", "{generation_1}"),
                     ("{binding_1, binding_2}", "{binding_1}")]:
        config = config.replace(old, new)
    config = config.replace("    Safety", "    ResidentSynchronizationObservesPhysicalOwner")
    anchor = "LET observed == TRUE"
    assert projection.count(anchor) == 1
    check_tlc_model_mutations(tlc_jar, "SessionEnvironmentProjection", projection, config,
        [(projection.replace(anchor, "LET observed == FALSE"), config,
          "ResidentSynchronizationObservesPhysicalOwner")])
    convergence = (ROOT / "formal/tla/SessionEnvironmentConvergence.cfg").read_text()
    # R1 no root ordering admits late A; R2 treating a descriptor as a physical
    # owner strands the Coordinator; R3 bypassing real ownership publishes B
    # over a retained A. Only named semantic counterexamples count as kills.
    order = "incoming.revision > before.revision \\/ incoming = before"
    compatible = "PhysicalProjectionCompatible(physical, incoming)"
    reject_descriptor = projection.replace("                       " + compatible,
        "                       " + compatible + ' /\\ physical # "none"', 1)
    bypass_owner = projection.replace("                       " + compatible,
                                      "                       TRUE", 1)
    check_tlc_model_mutations(tlc_jar, "SessionEnvironmentProjection", projection, convergence, [
        (projection.replace(order, "TRUE", 1), convergence, "ProjectionConvergenceSafety"),
        (reject_descriptor, convergence, "temporal:ProjectionEventuallyConverges"),
        (bypass_owner, convergence, "ProjectionConvergenceSafety"),
    ])
    print("Session observation model mutations passed, including resident physical observation; publication uses the canonical renewal gate")


def check_observation_trace_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    from render_runtime_refinement_traces import render_observations, module_name
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    # X1 bypass binding stamping; X2 resample processed time; X3 publish retired;
    # X4 rewrite recovered metadata. Mutate REAL emitted observations, not the
    # model. Each must violate the exact relation against a green trace baseline.
    for name in ["binding-apply", "processed-first-replay-terminal", "publication-cardinality-renewal", "store-torn-prefix", "environment-projection-convergence"]:
        document = json.loads((trace_dir / f"{name}.json").read_text())
        with tempfile.TemporaryDirectory(prefix="awaken-observation-mutant-") as temp:
            directory = Path(temp)
            raw, rendered = directory / "raw", directory / "rendered"
            raw.mkdir()
            path = raw / f"{name}.json"
            write_json(path, document)
            render_observations(raw, rendered, ROOT, manifest, False)
            module = module_name(name)
            baseline = (rendered / f"{module}.tla").read_text()
            config = (rendered / f"{module}.cfg").read_text()
            family = document["family"]
            if family == "root":
                step = document["steps"][0]
                step["after"]["updated"] = step["before"]["updated"]
            elif family == "processed":
                document["steps"][0]["after"]["time"] += 1
            elif family == "publication":
                step = next(s for s in document["steps"] if s["drained"])
                step["published"].append(step["drained"][0])
            elif family == "environment-projection":
                step = next(s for s in document["steps"] if s["rule"] == "p0-stale")
                step["after"]["binding"] = "A"
            else:
                document["steps"][0]["recovered"][0]["time"] += 1
            write_json(path, document)
            render_observations(raw, rendered, ROOT, manifest, False)
            mutant = (rendered / f"{module}.tla").read_text()
            assert mutant != baseline
            definition = manifest["session_observations"]["families"][family]
            check_tlc_model_mutations(tlc_jar, module, baseline, config,
                [(mutant, config, "constant:TraceRefines")], dependencies=(definition["model"], *definition["dependencies"]))
    check_observation_rule_mutants(trace_dir, manifest)
    check_realization_trace_mutants(tlc_jar, trace_dir, manifest)
    check_maintenance_observation_mutants(tlc_jar, trace_dir)
    check_deletion_observation_mutants(tlc_jar, trace_dir)
    check_event_batch_observation_mutants(tlc_jar, trace_dir)
    check_realization_capacity_observation_mutants(tlc_jar, trace_dir)
    print("Production observation trace mutation score: 5/5 base families plus maintenance, deletion and event-batch controls killed")

def check_acp_boundary_model_mutants(tlc_jar: Path) -> None:
    # One baseline and mutation owner for the strict Cowork gate and focused
    # checks: prompt-before-handshake, observer drop, fabricated ACK, duplex
    # deadlock, failed-TERM escalation veto, and reachable error-then-KILL /
    # timeout-then-exit witnesses.
    # Cause/effect: retained owner + timeout + actual exit must permit ACK; the
    # deliberately false invariant must fail, preventing vacuous recovery claims.
    source = (ROOT / "formal/tla/AcpBoundary.tla").read_text()
    config = (ROOT / "formal/tla/AcpBoundary.cfg").read_text()
    timeout = source[source.index("CleanupTimeout =="):source.index("ObserveExit(code) ==")]
    mutant_timeout = timeout.replace("killSent, observing, ack", "killSent, ack") + "  /\\ observing' = FALSE\n"
    check_tlc_model_mutations(tlc_jar, "AcpBoundary", source, config, [
        (source.replace("/\\ handshake\n  /\\ ~permissionCall", "/\\ ~permissionCall", 1),
         config.replace("INVARIANTS Safety", "INVARIANTS HandshakeBeforePrompt"),
         "HandshakeBeforePrompt"),
        (source.replace(timeout, mutant_timeout), config.replace("INVARIANTS Safety", "INVARIANTS CleanupObserverRetained Safety"), "CleanupObserverRetained"),
        (source.replace("/\\ exitCode # 0 /\\ observing /\\ ~ack", "/\\ termSent /\\ observing /\\ ~ack"), config.replace("INVARIANTS Safety", "INVARIANTS NoFabricatedQuiescence Safety"), "NoFabricatedQuiescence"),
        (source.replace('/\\ wireOutcome = "Active" /\\ downlink > 0', '/\\ wireOutcome = "Active" /\\ downlink > 0 /\\ hostLeft = 0'), config.replace("INVARIANTS Safety", "INVARIANTS WireCanAdvance Safety"), "WireCanAdvance"),
        (source.replace("/\\ termSent /\\ ~killSent /\\ exitCode = 0",
                        "/\\ termSent /\\ ~termFailed /\\ ~killSent /\\ exitCode = 0", 1),
         config.replace("INVARIANTS Safety", "INVARIANTS TermFailureAllowsEscalation Safety"), "TermFailureAllowsEscalation"),
        (source, config.replace("INVARIANTS Safety", "INVARIANTS NoFailedTermEscalationReached Safety"),
         "NoFailedTermEscalationReached"),
        (source, config.replace("INVARIANTS Safety", "INVARIANTS NoLateExitAcknowledged Safety"),
         "NoLateExitAcknowledged"),
    ])

def check_thread_commit_return_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect table for the existing raw-return and completion boundary:
    # R1 a delegated preparation refusal can durably retain Returned/Open with
    # zero attempts; the deliberately false witness must be refuted. R2/R3
    # folding that result, with/without batch finalization, closes its Open
    # relationship. Independently removing either closure must violate the
    # original StateSafety. Preserve the complete CFG and shared strict oracle;
    # parse errors, tool failures and timeouts never count as semantic detection.
    source = (ROOT / "formal/tla/ThreadCommitProjection.tla").read_text()
    config = (ROOT / "formal/tla/ThreadCommitProjection.cfg").read_text()
    closure = '[s.linkState EXCEPT ![c] = IF @ = "Open" THEN "Completed" ELSE @]'
    mutations = []
    for start, end in (("CompleteImmediate(s, t, c) ==", "CompleteImmediateAndFinalize(s, t, c) =="),
                       ("CompleteImmediateAndFinalize(s, t, c) ==", "MarkIndeterminate(s, t, c) ==")):
        if source.count(start) != 1 or source.count(end) != 1:
            raise SystemExit("immediate-return mutation action anchor drifted")
        begin, finish = source.index(start), source.index(end)
        action = source[begin:finish]
        if action.count(closure) != 1:
            raise SystemExit("immediate-return relationship closure anchor drifted")
        mutant = source[:begin] + action.replace(closure, "s.linkState", 1) + source[finish:]
        mutations.append((mutant, config, "StateSafety"))
    witness = (
        '\nNoStagedZeroAttemptReturn ==\n'
        '    ~\\E c \\in AgentCalls:\n'
        '        /\\ state.callState[c] = "Returned"\n'
        '        /\\ state.attempts[c] = 0\n'
        '        /\\ state.linkState[c] = "Open"\n'
    )
    if source.count("=============================================================================") != 1 or config.count("INVARIANT StateSafety") != 1:
        raise SystemExit("raw-return reachability witness anchor drifted")
    witness_source = source.replace("=============================================================================", witness + "=============================================================================", 1)
    witness_config = config.replace("INVARIANT StateSafety", "INVARIANTS StateSafety NoStagedZeroAttemptReturn", 1)
    mutations.append((witness_source, witness_config, "NoStagedZeroAttemptReturn"))
    check_tlc_model_mutations(tlc_jar, "ThreadCommitProjection", source, config, mutations,
                              report_metrics=True)
    print("Thread return projection: 2/2 immediate-fold mutants killed; zero-attempt staged return reachable")


def check_cowork_lifecycle_model_mutants(tlc_jar: Path) -> None:
    # Worker fault matrix W1 registry-only renewal; W2 evidence-only renewal;
    # W3 delayed probe success. Removing either expiry lane or erasing request
    # latency must violate its exact invariant; first establish the green model.
    source = (ROOT / "formal/tla/WorkerAuthorityEvidence.tla").read_text()
    config = (ROOT / "formal/tla/WorkerAuthorityEvidence.cfg").read_text()
    expiry = "(registryAge' < Window /\\ evidenceAge' < Window)"
    check_tlc_model_mutations(tlc_jar, "WorkerAuthorityEvidence", source, config, [
        (source.replace(expiry, "(registryAge' < Window)", 1), config, "BothProofsRequired"),
        (source.replace(expiry, "(evidenceAge' < Window)", 1), config, "BothProofsRequired"),
        (source.replace("evidenceAge' = probeAge", "evidenceAge' = 0", 1), config, "ProbeLatencyIncluded"),
    ])
    # W4 drain request + fair completion/ack progress => acknowledgement.
    # Reopening admission after acknowledgement must violate the existing fence.
    # Composition table: W4a reopening -> safety violation; W4b no ACK action
    # -> liveness violation even with startup/withdrawal fairness; W4c a valid
    # registration can reach ACK. The positive baseline checks both families.
    source = (ROOT / "formal/tla/WorkerDrain.tla").read_text()
    config = (ROOT / "formal/tla/WorkerDrain.cfg").read_text()
    check_tlc_model_mutations(tlc_jar, "WorkerDrain", source, config, [
        (source.replace('RepeatDrain == /\\ drainRequested /\\ gate = "Closed"\n               /\\ UNCHANGED vars',
                        'RepeatDrain == /\\ drainRequested /\\ gate = "Closed"\n'
                        '               /\\ gate\' = "Open"\n'
                        '               /\\ UNCHANGED <<claimInProgress, claimAttempts, claims, drainRequested, registryState, drainAcknowledged, attemptsAtRegistryDrain, claimsAtDrain, shutdown>>'),
         config, "RegistryDrainFollowsLocalFence"),
        (source.replace('AcknowledgeDrain == /\\ registryState = "Draining"',
                        'AcknowledgeDrain == /\\ FALSE /\\ registryState = "Draining"'),
         "SPECIFICATION ProgressSpec\nCONSTANT MaxClaims = 3\n"
         "PROPERTIES DrainEventuallyAcknowledged\n",
         "temporal:DrainEventuallyAcknowledged"),
        (source, "SPECIFICATION ProgressSpec\nCONSTANT MaxClaims = 3\n"
                 "INVARIANTS NoDrainAcknowledgedReached\n",
         "NoDrainAcknowledgedReached"),
        # G2 old normal-shutdown bug: abandon active work after the grace period
        # without requesting cancellation or retaining the bounded observer.
        (source.replace('(shutdown.cancelRequested /\\ shutdown.cleanupExpired)',
                        'shutdown.graceExpired', 1),
         config, "ShutdownDoesNotDiscardUncancelledWork"),
        # G3 cancellation/timeout is not evidence of physical return.
        (source.replace('!.quiesced = (shutdown.active = {})', '!.quiesced = TRUE', 1),
         config, "NoFabricatedQuiescence"),
    ])
    # C1 permit prompt before handshake; C2 restore timeout-as-observer-drop;
    # C3 TERM-as-ACK; C4 crash-as-ACK; C5 first-target-as-complete-fanout. Each
    # mutant must fail its independent invariant after a green baseline, never a
    # syntax/build/infrastructure error.
    check_acp_boundary_model_mutants(tlc_jar)
    source = (ROOT / "formal/tla/SessionThreadToolComposition.tla").read_text()
    config = (ROOT / "formal/tla/SessionThreadToolComposition.cfg").read_text()
    crash = source[source.index("CrashChild(run) =="):source.index("ObserveProcessQuiescence(run) ==")]
    mutant = crash.replace("UNCHANGED <<toolState, physicalRuns, invokedAttempt>>", "UNCHANGED <<toolState, invokedAttempt>>\n    /\\ physicalRuns' = physicalRuns \\ {run}")
    check_tlc_model_mutations(tlc_jar, "SessionThreadToolComposition", source, config, [
        (source.replace(crash, mutant), config.replace("    Safety", "    CrashDoesNotProveQuiescence\n    Safety"), "CrashDoesNotProveQuiescence"),
    ], dependencies=("ThreadCommitProjection",))
    check_thread_commit_return_model_mutants(tlc_jar)
    source = (ROOT / "formal/tla/ManagedCredentialRollout.tla").read_text()
    # Safety mutation does not assume eventual provider availability. The full
    # temporal configuration is separately selected by the strict gate.
    config = "SPECIFICATION Spec\nCONSTANT MaxRevision = 3\nINVARIANTS AdoptionCoversEveryNamedFence RolloutSafety\nCHECK_DEADLOCK FALSE\n"
    guard = r"\A target \in Targets: serviceRevision[target] >= deliveryEvent.revision"
    mutant = source.replace(guard, "serviceRevision[1] >= deliveryEvent.revision", 1)
    check_tlc_model_mutations(tlc_jar, "ManagedCredentialRollout", source, config, [
        (mutant, config, "AdoptionCoversEveryNamedFence"),
    ])
    check_session_deletion_model_mutants(tlc_jar)
    # P0: current errors stay visible under active aggregate authority; neither
    # error presentation nor a resolving tool reply can authorize new input.
    source = (ROOT / "formal/tla/SessionRuntimeProjection.tla").read_text()
    config = (ROOT / "formal/tla/SessionRuntimeProjection.cfg").read_text().replace(
        "    Safety", "    ErrorIsVisibleWithoutRelease\n    ActiveAggregateNeverPresentsSend\n    Safety")
    check_tlc_model_mutations(tlc_jar, "SessionRuntimeProjection", source, config, [
        (source.replace('ELSE IF phase = "error" THEN "error"',
                        'ELSE IF phase = "error" THEN aggregate', 1),
         config, "ErrorIsVisibleWithoutRelease"),
        (source.replace('ClientPresentedCanSend ==\n    aggregate = "idle" /\\ phase',
                        'ClientPresentedCanSend == phase', 1),
         config, "ActiveAggregateNeverPresentsSend"),
    ])
    # O1 later Running checkpoints followed by a close must not change the
    # opening already observed by a warm client. The cold max-at-terminal
    # mutant recreates the lost approval cursor from the real restart matrix.
    opening_config = (ROOT / "formal/tla/SessionRuntimeProjectionOpening.cfg").read_text()
    check_tlc_model_mutations(tlc_jar, "SessionRuntimeProjection", source, opening_config, [
        (source.replace("cursor <= other", "IF phase = \"closed\" THEN cursor >= other ELSE cursor <= other", 1),
         opening_config, "OpeningIdentityStable"),
        # O2 a terminal-only first interval cannot borrow a later interval's
        # Running cursor; both warm and cold readers must retain its fallback.
        (source.replace('IF aggregate = "terminal-only" THEN 1 ELSE',
                        'IF aggregate = "terminal-only" THEN sourceVersion ELSE', 1),
         opening_config, "OpeningIdentityStable"),
        # O3 completed cold reconstruction returning no opening differs from
        # not having read yet. The successful-read flag must reject omission.
        (source.replace("projectedVersion' = OpeningChoice", "projectedVersion' = 0", 1),
         opening_config, "OpeningIdentityStable"),
    ])
    print("Cowork lifecycle mutation score: 22/22 killed (worker evidence 3, drain 4, ACP/composition/rollout 6, cleanup evidence 4, presentation/admission 2, opening identity 3); drain ACK witness reachable")


def check_observation_rule_mutants(trace_dir: Path, manifest: dict) -> None:
    from copy import deepcopy
    from render_runtime_refinement_traces import render_observations
    killed = 0
    # Q7 causes: every emitted decision rule independently absent, duplicated,
    # renamed, or replaced with another partition while retaining its label.
    # Effect: coverage rejects before TLC. All originals first render green.
    for name in manifest["session_observations"]["required_scenarios"]:
        original = json.loads((trace_dir / f"{name}.json").read_text())
        with tempfile.TemporaryDirectory() as temp:
            raw, output = Path(temp) / "raw", Path(temp) / "out"
            raw.mkdir()
            path = raw / f"{name}.json"
            write_json(path, original)
            render_observations(raw, output, ROOT, manifest, False)
            for index in range(len(original["steps"])):
                for fault in ["delete", "duplicate", "rename"]:
                    doc = deepcopy(original)
                    if fault == "delete": del doc["steps"][index]
                    elif fault == "duplicate": doc["steps"].append(doc["steps"][index])
                    else: doc["steps"][index]["rule"] = "unknown-rule"
                    write_json(path, doc)
                    try:
                        render_observations(raw, output, ROOT, manifest, False)
                    except ValueError as error:
                        if not any(text in str(error) for text in ["empty observation", "rule coverage drift"]): raise
                        killed += 1
                    else: raise SystemExit(f"coverage mutant survived: {name}/{index}/{fault}")
            if name in {"publication-cardinality-renewal", "binding-clock-cases", "store-torn-prefix", "realization-http-crashes", "delete-retry-lifecycle", "event-batch-recovery-retry"}:
                for index in range(len(original["steps"])):
                    doc = deepcopy(original)
                    doc["steps"][index] = deepcopy(original["steps"][(index + 1) % len(original["steps"])])
                    doc["steps"][index]["rule"] = original["steps"][index]["rule"]
                    write_json(path, doc)
                    try: render_observations(raw, output, ROOT, manifest, False)
                    except ValueError as error:
                        if "rule conditions drift" not in str(error): raise
                        killed += 1
                    else: raise SystemExit(f"relabeled partition survived: {name}/{index}")
    print(f"Observation rule coverage mutations: {killed}/{killed} killed")


def check_checkpoint_recovery_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect rules match execution/checkpoint_observation.rs: a failed,
    # foreign or interrupted observation cannot become fresh execution; blocked
    # observation cannot consume evidence; valid reads still resume. Require a
    # green baseline and exact failures/reachability, never parse errors/timeouts.
    source = (ROOT / "formal/tla/CheckpointRecovery.tla").read_text()
    config = (ROOT / "formal/tla/CheckpointRecovery.cfg").read_text()
    header = "SPECIFICATION Spec\nCONSTANT MaxChunks = 3\n"
    mutations = []
    anchor = 'Recover == /\\ phase = "crashed"\n           /\\ readStatus \\in AcceptedReads'
    if source.count(anchor) != 1:
        raise SystemExit("checkpoint observation mutation anchor drifted")
    for outcome in ("storage", "fenced", "wrong-run", "wrong-thread", "pending", "cancelled", "deadline"):
        broken = anchor.replace("AcceptedReads", f'AcceptedReads \\cup {{"{outcome}"}}')
        mutations.append((source.replace(anchor, broken),
                          header + "INVARIANTS UnknownObservationNeverExecutes\n",
                          "UnknownObservationNeverExecutes"))
    anchor = 'RejectRead == /\\ phase = "crashed"'
    if source.count(anchor) != 1:
        raise SystemExit("checkpoint rejection mutation anchor drifted")
    # Keep the assignment well formed: mutate only RejectRead's UNCHANGED tuple.
    reject = source[source.index(anchor):source.index("\nComplete ==", source.index(anchor))]
    broken = reject.replace('              /\\ UNCHANGED',
                            "              /\\ checkpointValid' = FALSE\n              /\\ UNCHANGED")
    broken = broken.replace("staged, checkpoint, checkpointValid,", "staged, checkpoint,")
    mutations.append((source.replace(reject, broken),
                      header + "INVARIANTS BlockedPreservesCheckpoint\n", "BlockedPreservesCheckpoint"))
    mutations.append((source.replace(anchor, anchor + " /\\ FALSE"), header +
                      "CHECK_DEADLOCK FALSE\nPROPERTIES ObservationSettles\n", "temporal:ObservationSettles"))
    for anchor, broken, invariant in [
        ('/\\ EvidencePermits /\\ recovery.entry \\in EffectEntries',
         '/\\ recovery.entry \\in EffectEntries', "UnknownPredecessorNeverExecutes"),
        ('/\\ readStatus \\in AcceptedReads /\\ EvidencePermits',
         '/\\ readStatus \\in AcceptedReads', "ControlReturnRequiresEvidence"),
        ('(recovery.evidence = "absent" /\\ ~savedValid)',
         '(recovery.evidence = "absent")', "RecoveredPartialRequiresEvidence"),
    ]:
        if source.count(anchor) != 1: raise SystemExit("entry evidence mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + f"INVARIANTS {invariant}\n", invariant))
    broken = reject.replace('              /\\ UNCHANGED',
                            '              /\\ recovery\' = [recovery EXCEPT !.quiescent = TRUE]\n              /\\ UNCHANGED')
    broken = broken.replace("savedValid, recovery>>", "savedValid>>")
    mutations.append((source.replace(reject, broken),
                      header + "INVARIANTS UncertainOutcomeVetoesPhysicalReturn\n", "UncertainOutcomeVetoesPhysicalReturn"))
    mutations.append((source.replace('ReadDeadline == ObserveRead("deadline")', 'ReadDeadline == FALSE'),
                      header + "CHECK_DEADLOCK FALSE\nPROPERTIES RecoveryReadSettles\n", "temporal:RecoveryReadSettles"))
    mutations.append((source.replace('phase = "controlled" /\\ ~recovery.acknowledged', 'phase \\in {"crashed", "blocked", "controlled"} /\\ ~recovery.acknowledged'),
                      header + "INVARIANTS ControlAckRequiresObservedEvidence\n", "ControlAckRequiresObservedEvidence"))
    for witness in ("NoExactResume", "NoAbsentResume", "NoStorageBlock", "NoFenceBlock", "NoDeadlineReadBlock",
                    "NoRunMismatchBlock", "NoThreadMismatchBlock", "NoCancelledReadBlock",
                    "NoUnknownResumeBlock", "NoUnknownHandoffBlock", "NoUnknownCancelBlock",
                    "NoUnknownStopBlock", "NoKnownControlReturn", "NoControlAckReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "CheckpointRecovery", source, config, mutations)
    print("Checkpoint recovery: 15/15 mutants killed; fourteen entry/observation witnesses reachable")




if __name__ == "__main__":
    main()
