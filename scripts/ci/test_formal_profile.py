"""Decision-table tests for the qualification shared by CI and packaging."""

import copy
import contextlib
import hashlib
import io
import json
import os
import re
import runpy
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
        # L1 each live literal positive memcmp override is admitted by the CLI
        # parity test below. L2 independently mutate each actual run_kani command:
        # missing opt-in, nonliteral/zero/negative bound, foreign loop, disabled
        # check or trailing option => reject before source inventory. Another
        # valid command must not hide a bad one. Reuse the checker's continuation
        # normalization; multiple independent owners may use the same override.
        root = Path(__file__).resolve().parents[2]
        gate = root / "scripts/ci/check_formal.sh"
        checker = root / "scripts/ci/check_formal_coverage.py"
        source = gate.read_text().replace("\\\n", " ")
        original_read = Path.read_text
        override = "--cbmc-args --unwindset memcmp.0:16"
        invocations = [line for line in source.splitlines()
                       if line.lstrip().startswith("run_kani ") and override in line]
        self.assertTrue(invocations)
        for invocation in invocations:
            self.assertEqual(invocation.count(override), 1)
            self.assertEqual(source.count(invocation), 1)
            candidates = [("missing-opt-in", invocation.replace("-Z unstable-options", "", 1))]
            for replacement in (
                "--cbmc-args --no-unwinding-checks",
                "--cbmc-args --unwindset memcmp.0:0",
                "--cbmc-args --unwindset memcmp.0:-1",
                "--cbmc-args --unwindset memcmp.0:$BOUND",
                "--cbmc-args --unwindset foreign.0:16",
                override + " --no-assertions",
            ):
                candidates.append((replacement, invocation.replace(override, replacement, 1)))
            for mutation, replacement in candidates:
                candidate = source.replace(invocation, replacement, 1)
                def read(path, *args, **kwargs):
                    return candidate if path == gate else original_read(path, *args, **kwargs)
                with self.subTest(package=invocation.split()[1], mutation=mutation), \
                        mock.patch.object(Path, "read_text", read), \
                        mock.patch.object(sys, "argv", [str(checker)]), \
                        contextlib.redirect_stderr(io.StringIO()), \
                        self.assertRaises(SystemExit) as raised:
                    runpy.run_path(str(checker), run_name="__main__")
                self.assertEqual(raised.exception.code, 1)

    def test_checked_profiles_bind_runtime_and_checker(self):
        # Cause/effect rules: P1 known profile + exact package + nonempty unique
        # allowed harnesses => one canonical argv, including the profile's fixed
        # solver and each literal loop bound. P2 unknown/dynamic profile, foreign
        # package/harness, duplicate/empty selection, appended check/solver flags
        # or repeated profile => reject before Cargo. P3 missing opt-in, changed
        # solver, loop, bound, order, count or flags => reject the raw argv too.
        # P4 the new fixed-solver harness cannot drop swap/solver and escape via
        # legacy memcmp-only acceptance, even under the wrong package. P5 qualified
        # Activity argv and all existing nonreserved memcmp calls stay accepted.
        # Execute the actual shell function with only its Cargo/deadline port
        # replaced by an argument recorder: rejection must return 2, never be
        # lost inside process substitution or launch a default solver.
        import formal_kani_options as options
        root = Path(__file__).resolve().parents[2]
        gate = (root / "scripts/ci/check_formal.sh").read_text()
        runner = gate[gate.index("  run_kani() {"):gate.index("  run_kani awaken-agent-contract")]
        # Frozen qualified hex/main-dependency argv, all allowed harnesses sorted.
        # Only the actual Session disambiguator changed; bounds/solver/order
        # are retained and independently audited on all seven Activity GOTOs.
        # Digest fixtures avoid duplicating the production loop-name catalog.
        activity_argv = {
            # The two-admission partitions left this profile when they moved onto
            # the two-slot member model; only the two real-B-tree bases remain.
            "activity-admission-height-v1": "e0d02fc78c77afaa13cc05e06786f5c85f1359626a80e02d0cf61909f81be947",
            # The terminal proof on the real set now makes one admission, not two;
            # the two-admission terminal table moved to the two-slot model.
            "activity-terminal-height-v1": "e02882ed1a14cd5972dc70aac464d95f507f94376573ebe1d983cad1d0f334fd",
        }
        # P6 the two encoder profiles pin independently qualified solvers and
        # exact UTF-8/align bounds; the generic P2/P3 mutations below must reject
        # every changed bound, missing loop and solver, not just malformed names.
        # P7 the resume profile binds actual empty-payload loops and recursion
        # symbols; the same argument mutations must reject any changed pin,
        # bound, solver, package or harness before invoking Cargo.
        # P8 Erasure selection/count retain fixed default-equivalent CaDiCaL
        # and their distinct actual-GOTO payload monomorphizations. Action reuses
        # the exact borrowed-token selection pins; its all-harness digest fixes
        # that added binding without allowing arbitrary harnesses. The same
        # P2/P3 matrix rejects cross-owner harnesses, dropped/changed bounds,
        # missing solver, and fallback to the legacy memcmp-only exception.
        # P9 successor reuses the five common actual-GOTO loops plus the old-row
        # all loop; its exact package/harness/default solver is subjected to every
        # P2/P3 negative control. A digest is argv qualification, not proof PASS.
        frozen_argv = {
            **activity_argv,
            "resume-validator-empty-object-v1": '4d0d7c52778ea2a3e569a5af1489e874b3e3b3fc81cc76e9d70c10060974abc0',
            'erasure-progress-selection-v1': 'cda63519e60138de09288bea3369e018f51160195fe33a2bbe4ff1b0791992db',
            'erasure-checkpoint-successor-v1': '6f65105a983ff7e3796ac4596398a4fe20989c684a0b377cbd924419529e9bf3',
            'erasure-progress-count-v1': '35f9880ae0c3ae92283dbf1236f5b98e8cb851c16111f01e3b3580e1fe982f45',
            "fingerprint-fnv-hex-v1": "714ec79c46a2150f9229bb0bf5df7335fe0126e40d244aa4bb21755b273f7816",
            "fingerprint-sha256-hex-v1": "b9d1b5eb4b805eb197237899983eba96dc2416216d089d815525d203889e0556",
        }
        for profile, (package, allowed, loops, solver) in options.PROFILES.items():
            harness = sorted(allowed)[0]
            selected = ["--checked-loop-profile", profile, "--harness", harness]
            expanded = options.expand_options(package, selected)
            cut = expanded.index("--cbmc-args")
            prefix, tail = expanded[:cut], expanded[cut:]
            self.assertTrue(options.checked_loop_override(package, prefix, tail, True))
            self.assertEqual(expanded[-1], ",".join(f"{name}:{bound}" for name, bound in loops))
            self.assertEqual(prefix[:4] if solver else prefix[:2],
                             ["-Z", "unstable-options", "--solver", solver] if solver
                             else ["-Z", "unstable-options"])
            for names in (sorted(allowed), sorted(allowed, reverse=True)):
                selections = [piece for name in names for piece in ("--harness", name)]
                all_argv = options.expand_options(package, selected[:2] + selections)
                all_cut = all_argv.index("--cbmc-args")
                self.assertTrue(options.checked_loop_override(
                    package, all_argv[:all_cut], all_argv[all_cut:], True))
                self.assertEqual(all_argv[4 if solver else 2:all_cut], selections)
                if profile in frozen_argv and names == sorted(allowed):
                    self.assertEqual(hashlib.sha256(json.dumps(
                        all_argv, separators=(",", ":")).encode()).hexdigest(), frozen_argv[profile])
            invalid = [
                ("foreign-package", "another-package", selected),
                ("unknown-profile", package, [selected[0], "unknown", *selected[2:]]),
                ("dynamic-profile", package, [selected[0], "$PROFILE", *selected[2:]]),
                ("foreign-harness", package, [*selected[:3], "foreign"]),
                ("empty", package, selected[:2]),
                ("duplicate-harness", package, selected + selected[2:]),
                ("duplicate-profile", package, selected + selected[:2]),
            ]
            for flag in ("--no-unwinding-checks", "--no-assertions", "--solver", "--cbmc-args"):
                invalid.append((flag, package, selected + [flag, "foreign"]))
            for label, candidate_package, candidate in invalid:
                with self.subTest(profile=profile, rule=label):
                    with self.assertRaises(ValueError):
                        options.expand_options(candidate_package, candidate)
            entries = tail[2].split(",")
            tail_mutations = [
                [*tail[:2], ",".join(entries + entries[:1])],
                [*tail[:2], ",".join(reversed(entries))],
                tail + ["--no-assertions"],
                ["--cbmc-args", "--no-unwinding-checks"],
                ["foreign", *tail[1:]],
            ]
            for index, (loop, bound) in enumerate(loops):
                tail_mutations.append([*tail[:2], ",".join(entries[:index] + entries[index + 1:])])
                for replacement in (f"foreign.0:{bound}", f"{loop}:0", f"{loop}:{bound - 1}",
                                    f"{loop}:{bound + 1}", f"{loop}:$BOUND"):
                    changed = entries.copy()
                    changed[index] = replacement
                    tail_mutations.append([*tail[:2], ",".join(changed)])
            for candidate in tail_mutations:
                with self.subTest(profile=profile, tail=candidate):
                    self.assertFalse(options.checked_loop_override(package, prefix, candidate, True))
            prefix_mutations = [prefix[2:], prefix + ["--no-assertions"],
                                prefix[:-1] + ["foreign"], prefix + ["--harness", harness],
                                prefix + ["--solver", "z3"]]
            if solver:
                prefix_mutations += [prefix[:2] + prefix[4:],
                                     prefix[:3] + ["minisat"] + prefix[4:],
                                     prefix[:3] + ["$SOLVER"] + prefix[4:]]
            for candidate in prefix_mutations:
                with self.subTest(profile=profile, prefix=candidate):
                    self.assertFalse(options.checked_loop_override(package, candidate, tail, True))
            self.assertFalse(options.checked_loop_override("foreign-package", prefix, tail, True))
            self.assertFalse(options.checked_loop_override(package, prefix, tail, False))
            if solver:
                legacy_tail = ["--cbmc-args", "--unwindset", "memcmp.0:2"]
                for candidate_package in (package, "foreign-package"):
                    for candidate_prefix in (prefix, prefix[:2] + prefix[4:]):
                        self.assertFalse(options.checked_loop_override(
                            candidate_package, candidate_prefix, legacy_tail, True), "P4")
            # A Z3 profile runs only against the pinned solver; stand one in so
            # this test observes argv, not the machine's solver installation.
            pinned_z3 = re.search(r'readonly z3_version="([^"]+)"', gate).group(1)
            solver_bin = Path(tempfile.mkdtemp())
            self.addCleanup(shutil.rmtree, solver_bin, True)
            (solver_bin / "z3").write_text(f"#!/bin/sh\necho '{pinned_z3}'\n")
            (solver_bin / "z3").chmod(0o755)
            shell = (f"z3_version='{pinned_z3}'; kani_jobs=1; kani_batch_timeout_seconds=600; CARGO_TARGET_DIR=/tmp/checked-profile-test\n"
                     "run_with_deadline() { printf '%s\\n' \"$@\"; }\n" + runner + '\nrun_kani "$@"')
            solver_env = dict(os.environ, PATH=f"{solver_bin}{os.pathsep}{os.environ['PATH']}")
            good = subprocess.run(["bash", "-c", shell, "profile-test", package, *selected],
                                  cwd=root, text=True, capture_output=True, env=solver_env)
            self.assertEqual(good.returncode, 0, good.stderr)
            self.assertEqual(good.stdout.splitlines()[9:], expanded)
            for label, candidate_package, candidate in invalid:
                with self.subTest(profile=profile, runtime=label):
                    bad = subprocess.run(["bash", "-c", shell, "profile-test", candidate_package, *candidate],
                                         cwd=root, text=True, capture_output=True, env=solver_env)
                    self.assertEqual(bad.returncode, 2, bad.stderr)
                    self.assertEqual(bad.stdout, "")
                    self.assertIn("formal Kani options:", bad.stderr)
        legacy_count = 0
        for command in gate.replace("\\\n", " ").splitlines():
            tokens = shlex.split(command, comments=True)
            if not tokens or tokens[0] != "run_kani" or "--unwindset" not in tokens:
                continue
            cut = tokens.index("--cbmc-args")
            if re.fullmatch(r"memcmp\.0:[1-9][0-9]*", tokens[-1]):
                self.assertTrue(options.checked_loop_override(tokens[1], tokens[2:cut], tokens[cut:], True), "P5")
                self.assertEqual(options.expand_options(tokens[1], tokens[2:]), tokens[2:])
                legacy_count += 1
        self.assertGreater(legacy_count, 0)

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
