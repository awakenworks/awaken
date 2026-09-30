"""Decision-table tests for the qualification shared by CI and packaging."""

import copy
import contextlib
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import tempfile
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

import check_feature_coverage
import formal_ledger

from formal_profile import guardrail_results, profile_result, resolve_obligations


def fixture():
    row = {
        "id": "run.safe", "formalizable": True,
        "claim_scope": "repository_safety", "proof_grade": "production_kernel_proved",
        "proof_harnesses": ["safe"], "production_entrypoints": ["crates/owner/src/lib.rs"],
    }
    feature = {
        "id": "run", "criticality": "P0",
        "requirements": [{"kind": "safety", "repository_controlled": True,
                          "obligation_prefixes": ["run"], "obligations": ["run.safe"]}],
    }
    return {"features": [feature]}, {"obligations": [row]}


class ProfileTests(unittest.TestCase):
    def test_evidence_namespaces_require_their_existing_product_journeys(self):
        # J1 explicit obligation / exact namespace + existing product journey
        # => complete. J2 remove its edge => reject the disconnected proof even
        # when model and Rust files still exist. J3 remove the journey stage =>
        # reject the disconnected feature. Cross J1-J3 with Worker authority,
        # Memory invalidation and retained Memory participants: a neighboring
        # `worker` / `memory` prefix cannot consume the underscore namespace.
        # Shared baseline and journey-stage controls run once per distinct
        # graph. This exercises the canonical checker with only JSON varied;
        # it verifies assurance wiring, not Rust refinement or model validity.
        checker = check_feature_coverage
        original_load = checker.load_json
        current = original_load(checker.FEATURES)
        with mock.patch.object(sys, "argv", [str(checker.__file__), "--require-complete"]), contextlib.redirect_stdout(io.StringIO()):
            checker.main()  # J1: one green baseline for all linked namespaces.
        checked_stages = set()
        for feature_id, requirement_id, field, edge, obligation in [
            ("worker.remote_execution", "remote_protocol", "obligations",
             "worker_authority.independent_evidence_never_extends_expired_proof",
             "worker_authority.independent_evidence_never_extends_expired_proof"),
            ("resource.memory", "memory_consistency", "obligation_prefixes",
             "memory_invalidation", "memory_invalidation.listener_cleanup_is_owned_and_joined"),
            ("resource.memory", "memory_consistency", "obligation_prefixes",
             "memory_participant", "memory_participant.live_provider_retains_pending_and_failed_operations"),
        ]:
            for rule in ("missing-obligation", "missing-stage"):
                if rule == "missing-stage":
                    if feature_id in checked_stages:
                        continue
                    checked_stages.add(feature_id)
                with self.subTest(obligation=obligation, rule=rule):
                    features = copy.deepcopy(current)
                    feature = next(row for row in features["features"] if row["id"] == feature_id)
                    requirement = next(row for row in feature["requirements"] if row["id"] == requirement_id)
                    self.assertIn(edge, requirement[field])
                    if rule == "missing-obligation":
                        requirement[field].remove(edge)
                    elif rule == "missing-stage":
                        for journey in features["product_journeys"]:
                            journey["stages"] = [stage for stage in journey["stages"] if stage != feature_id]
                    def load(path):
                        return features if path == checker.FEATURES else original_load(path)
                    errors = io.StringIO()
                    with mock.patch.object(checker, "load_json", side_effect=load), mock.patch.object(sys, "argv", [str(checker.__file__), "--require-complete"]), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(errors):
                        with self.assertRaises(SystemExit) as stopped:
                            checker.main()
                        self.assertEqual(stopped.exception.code, 1)
                        self.assertIn(obligation if rule == "missing-obligation" else feature_id, errors.getvalue())

    def test_readme_does_not_duplicate_live_proof_measurements(self):
        # D1 source/model changes make copied proof counts and state-space tables
        # stale; D2 the ledger CLI and exact-invocation reports remain canonical.
        # Require D2 and reject both old D1 forms. This is a documentation drift
        # gate, not evidence that the displayed invocation actually passed.
        root = Path(__file__).resolve().parents[2]
        text = (root / "formal/README.md").read_text()
        self.assertIn("check_formal_coverage.py --json", text)
        self.assertIn("target/test-timings/formal-run.json", text)
        self.assertIsNone(re.search(r"(?m)^- .*: [0-9]+/[0-9]+\.", text))
        self.assertNotIn("| Model | Generated | Distinct |", text)

    def test_requirement_selection_is_exact_and_deduplicated(self):
        # C1 criticality=P0/P1/P2; C2 safety/other; C3 self/external owner.
        # R1 only P0+safety+self contributes. A mandatory anchor keeps the
        # denominator nonempty, independently exposing all 12 selection rows.
        for criticality in ("P0", "P1", "P2"):
            for kind in ("safety", "functional"):
                for controlled in (True, False):
                    with self.subTest(criticality=criticality, kind=kind, controlled=controlled):
                        features, coverage = fixture()
                        candidate = copy.deepcopy(features["features"][0])
                        candidate.update(id="candidate", criticality=criticality)
                        candidate["requirements"][0].update(
                            kind=kind, repository_controlled=controlled,
                            obligation_prefixes=["candidate"], obligations=["candidate.safe"],
                        )
                        features["features"].append(candidate)
                        coverage["obligations"].append(dict(coverage["obligations"][0], id="candidate.safe"))
                        selected = criticality == "P0" and kind == "safety" and controlled
                        result = profile_result(features, coverage)
                        self.assertEqual(result["required_obligations"], 1 + selected)
                        self.assertTrue(result["eligible"])

    def test_missing_duplicate_external_and_empty_denominators_fail(self):
        # C1 valid baseline; C2 each independently malformed reference/domain.
        # R1 C1=>qualification; R2 any C2=>error, never vacuous eligibility.
        def duplicate(f, c):
            c["obligations"].append(dict(c["obligations"][0]))
        def missing(f, c):
            f["features"][0]["requirements"][0]["obligations"] = ["missing"]
        def prefix(f, c):
            f["features"][0]["requirements"][0]["obligation_prefixes"] = ["ru"]
        def empty(f, c):
            f["features"] = []
        def external(f, c):
            c["obligations"][0].update(formalizable=False, claim_scope="external_assumption")
        def empty_requirement(f, c):
            f["features"][0]["requirements"][0].update(obligation_prefixes=[], obligations=[])
        for mutation in (duplicate, missing, prefix, empty, external, empty_requirement):
            with self.subTest(mutation=mutation.__name__):
                features, coverage = fixture()
                mutation(features, coverage)
                with self.assertRaises(ValueError):
                    profile_result(features, coverage)

    def test_proof_grade_does_not_promote_models_traces_or_missing_harnesses(self):
        # C1 direct/model/theorem/trace/unknown; C2 direct inputs complete.
        # R1 direct+C2 qualifies structurally (not proof-run success); R2 model
        # and theorem remain gaps; R3 trace relabeling/unknown/incomplete errors.
        for grade in ("production_kernel_proved", "abstract_model_checked",
                      "abstract_theorem_proved", "implementation_refined", "invented"):
            with self.subTest(grade=grade):
                features, coverage = fixture()
                coverage["obligations"][0]["proof_grade"] = grade
                if grade in {"implementation_refined", "invented"}:
                    with self.assertRaises(ValueError):
                        profile_result(features, coverage)
                else:
                    result = profile_result(features, coverage)
                    self.assertEqual(result["eligible"], grade == "production_kernel_proved")
                    self.assertEqual(result["gaps"], [] if result["eligible"] else ["run.safe"])
        for field in ("proof_harnesses", "production_entrypoints"):
            features, coverage = fixture()
            coverage["obligations"][0].pop(field)
            with self.subTest(field=field), self.assertRaises(ValueError):
                profile_result(features, coverage)

    def test_direct_grade_rejects_stale_whole_row_abstract_claims(self):
        # Causes: C1 direct/non-direct grade; C2 bounds describe either the
        # whole row as still abstract/model-linked or only an explicit residual
        # subsystem boundary. Effects: R1 direct+C2-whole rejects contradictory
        # authority; R2 direct+C2-residual remains valid; R3 a model grade may
        # accurately retain its abstract description. This lint changes no
        # proof result and does not infer scope from ordinary words such as
        # "abstract state" or from a named external limitation.
        features, coverage = fixture()
        row = coverage["obligations"][0]
        for obsolete in (
            "The whole obligation remains abstract-model checked.",
            "The whole obligation is not upgraded.",
        ):
            row["bounds"] = [obsolete]
            with self.subTest(obsolete=obsolete), self.assertRaisesRegex(
                ValueError, "contradicts"
            ):
                profile_result(features, coverage)

        row["bounds"] = [
            "Fair scheduling and external termination remain abstract or external evidence."
        ]
        self.assertTrue(profile_result(features, coverage)["eligible"])

        row["proof_grade"] = "abstract_model_checked"
        row["bounds"] = ["The row remains abstract_model_checked."]
        self.assertFalse(profile_result(features, coverage)["eligible"])

    def test_prefix_does_not_capture_similar_names_or_external_assumptions(self):
        # C1 exact dotted prefix; C2 lexical neighbor; C3 external member.
        # Only formalizable C1 is selected; explicit identities remain exact.
        rows = {"run.safe": {"formalizable": True},
                "runner.safe": {"formalizable": True},
                "run.external": {"formalizable": False}}
        self.assertEqual(resolve_obligations({"obligation_prefixes": ["run"]}, rows, "owner"), {"run.safe"})

    def test_target_guardrail_profiles_reuse_exact_feature_requirements(self):
        # Cause/effect graph: C1 exact feature/requirement coordinate exists;
        # C2 it is repository-controlled safety; C3 its obligation is direct;
        # C4 deployment assumptions are declared. E1 produces one deduplicated
        # diagnostic without owning target status. E2 C3 false retains the exact
        # gap. E3 missing/duplicate/malformed coordinates or C2 false reject.
        #
        # | Rule | C1 | C2 | C3 | C4 | Effect |
        # |---|---|---|---|---|---|
        # | G1 | T | T | T | T | eligible + assumption |
        # | G2 | T | T | F | T | exact gap |
        # | G3 | F | * | * | * | reject |
        # | G4 | T | F | * | * | reject |
        features, coverage = fixture()
        features["features"][0]["requirements"][0]["assumptions"] = ["external.runtime"]
        features["guardrail_promotions"] = [{
            "id": "G42",
            "requirements": [{"feature": "run", "requirement": None}],
        }]
        # The fixture requirement needs a stable id because target profiles use
        # exact coordinates instead of repeating obligation prefixes.
        features["features"][0]["requirements"][0]["id"] = "safety"
        features["guardrail_promotions"][0]["requirements"][0]["requirement"] = "safety"
        direct = profile_result(features, coverage)["guardrails"]["G42"]
        self.assertTrue(direct["eligible"], "G1/E1")
        self.assertEqual(direct["deployment_assumptions"], ["external.runtime"], "G1/E1")

        coverage["obligations"][0]["proof_grade"] = "abstract_model_checked"
        modeled = profile_result(features, coverage)["guardrails"]["G42"]
        self.assertEqual(modeled["gaps"], ["run.safe"], "G2/E2")

        for mutation in (
            lambda value: value["guardrail_promotions"][0]["requirements"][0].update(requirement="missing"),
            lambda value: value["guardrail_promotions"][0]["requirements"].append(
                {"feature": "run", "requirement": "safety"}
            ),
            lambda value: value["guardrail_promotions"][0]["requirements"][0].update(extra=True),
            lambda value: value["features"][0]["requirements"][0].update(kind="functional"),
        ):
            candidate = copy.deepcopy(features)
            mutation(candidate)
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                guardrail_results(candidate, {"run.safe": coverage["obligations"][0]})

    def test_strict_formal_runner_requires_exact_g42_and_g43_guardrails(self):
        # Cause/effect decision table: C1 the sole coverage invocation exists;
        # C2 evidence completeness is required; C3 exact G42 is required once;
        # C4 exact G43 is required once. R1 all true=>the strict runner is wired;
        # R2 any false, duplicate, renamed, or parallel invocation=>this source
        # test fails before tools run. Runtime proof execution remains owned by
        # check_formal.sh and its exact-SHA formal-run receipt.
        root = Path(__file__).resolve().parents[2]
        source = (root / "scripts/ci/check_formal.sh").read_text()
        invocations = []
        for command in source.replace("\\\n", " ").splitlines():
            tokens = shlex.split(command, comments=True)
            if tokens[:2] == ["python3", "scripts/ci/check_formal_coverage.py"]:
                invocations.append(tokens)
        self.assertEqual(
            invocations,
            [[
                "python3",
                "scripts/ci/check_formal_coverage.py",
                "--require-complete",
                "--require-guardrail",
                "G42",
                "--require-guardrail",
                "G43",
            ]],
            "R1 requires one exact fail-closed G42/G43 qualification boundary",
        )

    def test_absent_guardrail_profiles_preserve_legacy_qualification(self):
        # Cause/effect graph: C1 promotion field is absent or an empty list;
        # C2 promotion field has the wrong type. E1 legacy feature requirements
        # remain valid without inventing guardrail diagnostics. E2 malformed
        # opt-in configuration fails before feature requirement indexing.
        #
        # | Rule | C1 | C2 | Effect |
        # |---|---|---|---|
        # | L1 | T | F | qualify with empty guardrails |
        # | L2 | F | T | reject malformed configuration |
        for declaration in (None, []):
            features, coverage = fixture()
            if declaration is not None:
                features["guardrail_promotions"] = declaration
            with self.subTest(declaration=declaration):
                self.assertEqual(profile_result(features, coverage)["guardrails"], {}, "L1/E1")

        features, coverage = fixture()
        features["guardrail_promotions"] = {}
        with self.assertRaises(ValueError):
            profile_result(features, coverage)

    def test_kani_loop_override_cannot_disable_or_redirect_checks(self):
        # L1 a harness tagged with a real memcmp profile is planned with exactly
        # that profile's argv. L2 a tag cannot carry Kani or CBMC options: a
        # check-disabling flag, a nonliteral/zero/foreign loop, a trailing
        # option or an unknown key is refused before any batch exists -- the
        # only option source is an audited profile, never the tag text.
        import formal_plan
        from formal_kani_options import serialize
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "crates/a/src").mkdir(parents=True)
            (root / "crates/a/Cargo.toml").write_text('[package]\nname = "a"\nversion = "0.0.0"\n')
            source = root / "crates/a/src/lib.rs"

            def tagged(tag: str) -> None:
                source.write_text(f"#[cfg(kani)]\n/// @obligation run.safe\n/// @kani {tag}\n#[kani::proof]\nfn proof() {{}}\n")

            tagged("profile=unwind-memcmp-2-v1")
            [batch] = formal_plan.batches(root, "full")
            self.assertEqual(batch.argv(), ["a", *serialize("unwind-memcmp-2-v1", "a", ["--exact", "--harness", "proof"])], "L1")
            for mutation in (
                "profile=unwind-memcmp-2-v1 --no-unwinding-checks",
                "profile=unwind-memcmp-2-v1 cbmc-args=--no-assertions",
                "profile=unwind-memcmp-0-v1",
                "profile=$PROFILE",
                "profile=unwind-memcmp-2-v1 profile=solver-z3-v1",
                "features=x;--no-assertions",
                "unwindset=foreign.0:16",
            ):
                tagged(mutation)
                with self.subTest(mutation=mutation), self.assertRaises(formal_plan.PlanError):
                    formal_plan.batches(root, "full")

    def test_checked_profiles_bind_runtime_and_checker(self):
        # Cause/effect rules: P1 a known profile serializes one canonical argv
        # around the harness filter -- `-Z unstable-options`, its fixed solver,
        # the filter, then its literal loop bounds; a solver-only profile is
        # the solver and the filter. P2 an unknown profile, or a
        # package-bound profile used from another package, is refused before
        # any Cargo call. P3 the pinned argv of every profile: a changed loop,
        # bound, order or solver changes the digest, so it cannot drift silently.
        # P4 the actual shell runner hands every planned batch to Cargo verbatim
        # (only its Cargo/deadline port replaced by an argument recorder); a
        # failing or empty plan starts no batch and fails the gate.
        import formal_kani_options as options
        root = Path(__file__).resolve().parents[2]
        gate = (root / "scripts/ci/check_formal.sh").read_text()
        frozen = {
            "resume-validator-empty-object-v1": "c0c52971b7645cf31c694554ca286f520d10af37787c2ec966af176a906d55e1",
            "erasure-progress-selection-v1": "4052ded580c29bca71f8cbd73a747fa1b9d85d43c709b8006fbaf5313652012d",
            "erasure-checkpoint-successor-v1": "012c22d237f41cee1953bc1322707632f45f88c4981c0499b1f1b49645a95d91",
            "fingerprint-fnv-hex-v1": "999182642c04b9eeb4fdff6a51aab78f91f31203383015d1e5c044481ce8e9c3",
            "fingerprint-sha256-hex-v1": "f92d9f7a6e8af73cb22340453f27f9f73ca1ad3eba3ce2f6fdd056649064fdf0",
            "activity-admission-height-v1": "efd7d74749d4fae65c9f888e0b7c94f074979a1b97b497b8497e7f8b084df65f",
            "activity-terminal-height-v1": "656be82c5090279133b890987147325ce4c38eadde056584c279713b14284a53",
            "retained-accepted-two-replay-v1": "761f6133478b066ef75c572cab15e8a5ba03b57f69be3fb672173fc3c4626a7e",
            "unwind-memcmp-2-v1": "7a47708ca2fa05ed3db19ffd5d390f8b60287aaff91947ce238a4c182a31530b",
            "unwind-memcmp-64-v1": "46eaff90bd3f7b0d469920c6345f769512f3c33ea4de6b29777ecd0f5e6e6588",
            "unwind-memcmp-65-v1": "d6e88e38c1eab25178f2325c4f55ed94596a4b02999dcdf40d05d8861d15c0d6",
            "minisat-unwind-memcmp-16-v1": "8407b4e9522cef116eac9248c14f3bc4c8dc3283572588127627ec018b5a6de6",
            "solver-cadical-v1": "089b9f686c4cc920ae364668d934123efed8d2c59c153b0599433eb95938dc3d",
            "solver-kissat-v1": "300a4cc29798d5984126f705cf0ff7f13b08a4f20fa31de0e8750eab2ecea038",
            "solver-minisat-v1": "a2ad2c3fc1b6f395326c604172d2521a1d31d7b1d3d41ad2dc8d3bf4e96cc1d4",
            "solver-z3-v1": "62e2845cd585558b237717cd6ebc4b2eb5298b83ef95a777af5c035f07ea08ee",
        }
        self.assertEqual(set(frozen), set(options.PROFILES), "P3 every profile is pinned")
        selection = ["--exact", "--harness", "module::harness"]
        for name, profile in options.PROFILES.items():
            package = profile.package or "any-package"
            with self.subTest(profile=name):
                argv = options.serialize(name, package, selection)
                solver = ["--solver", profile.solver] if profile.solver else []
                if profile.loops:
                    self.assertEqual(argv, ["-Z", "unstable-options", *solver, *selection, "--cbmc-args",
                                            "--unwindset", ",".join(f"{loop}:{bound}" for loop, bound in profile.loops)], "P1")
                else:
                    self.assertEqual(argv, [*solver, *selection], "P1")
                pinned = hashlib.sha256(json.dumps(options.serialize(name, package, []),
                                                   separators=(",", ":")).encode()).hexdigest()
                self.assertEqual(pinned, frozen[name], "P3")
                if profile.package is not None:
                    with self.assertRaisesRegex(ValueError, "is bound to package"):
                        options.serialize(name, "foreign-package", selection)
        with self.assertRaisesRegex(ValueError, "unknown Kani profile"):
            options.serialize("unknown-v1", "any-package", selection)

        runner = gate[gate.index("  run_kani() {"):gate.index("  # Present and exactly the pinned version")]
        loop = gate[gate.index('  kani_plan="$('):gate.index("  done\n", gate.index('  kani_plan="$(')) + 7]
        pinned_z3 = re.search(r'readonly z3_version="([^"]+)"', gate).group(1)
        solver_bin = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, solver_bin, True)
        (solver_bin / "z3").write_text(f"#!/bin/sh\necho '{pinned_z3}'\n")
        (solver_bin / "z3").chmod(0o755)
        z3_check = gate[gate.index("  z3_is_pinned() {"):gate.index("  }\n", gate.index("  z3_is_pinned() {")) + 4]
        planned = ["pkg-a --exact --harness a::one --harness a::two",
                   "pkg-b --lib --features x -Z unstable-options --solver z3 --exact --harness b --cbmc-args --unwindset memcmp.0:2"]
        for label, plan, status in (
            ("batches", "printf '%s\\n' " + " ".join(shlex.quote(line) for line in planned), 0),
            ("failing plan", "return 3", 3),
            ("empty plan", "return 0", 1),
        ):
            shell = (f"set -euo pipefail\nz3_version='{pinned_z3}'; kani_jobs=1; kani_batch_timeout_seconds=600; "
                     "CARGO_TARGET_DIR=/tmp/checked-profile-test; missing=0\n"
                     "formal_selection=(--tier full); formal_affected=''\n"
                     "awaken_configure_kani_driver_target() { :; }\n"
                     "run_with_deadline() { printf 'CALL'; printf ' %s' \"$@\"; printf '\\n'; }\n"
                     f"python3() {{ {plan}; }}\n" + runner + z3_check + loop)
            with self.subTest(runner=label):
                result = subprocess.run(["bash", "-c", shell], cwd=root, text=True, capture_output=True,
                                        env=dict(os.environ, PATH=f"{solver_bin}{os.pathsep}{os.environ['PATH']}"))
                self.assertEqual(result.returncode, status, result.stderr)
                calls = [line for line in result.stdout.splitlines() if line.startswith("CALL ")]
                if status:
                    self.assertEqual(calls, [], "P4 no batch starts without a plan")
                else:
                    self.assertEqual(calls, [
                        f"CALL 600 cargo kani -p {line.split()[0]} --output-format terse -j 1 "
                        + " ".join(line.split()[1:]) for line in planned
                    ], "P4 every planned batch reaches Cargo verbatim")

    def test_live_checker_and_release_consume_the_same_qualification(self):
        # One real ledger is consumed through the CLI and release imports.
        # R1 their denominator, numerator and exact gaps agree, with no mocked
        # qualification function or hardcoded counter acting as the oracle.
        root = Path(__file__).resolve().parents[2]
        sys.path.insert(0, str(root / "scripts/release"))
        import verification_manifest
        self.assertIs(verification_manifest.profile_result, profile_result)
        features = json.loads((root / "formal/features.json").read_text())
        coverage = formal_ledger.load(root)
        result = profile_result(features, coverage)
        checked = json.loads(subprocess.check_output(
            [sys.executable, str(root / "scripts/ci/check_formal_coverage.py"), "--json"], text=True
        ))["verified_profile"]
        self.assertEqual(checked["required"], result["required_obligations"])
        for key in ("directly_proved_or_refined", "eligible", "gaps"):
            self.assertEqual(checked[key], result[key])


if __name__ == "__main__":
    unittest.main()
