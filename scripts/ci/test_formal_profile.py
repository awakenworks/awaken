"""Decision-table tests for the qualification shared by CI and packaging."""

import copy
import json
import re
from pathlib import Path
import subprocess
import sys
import unittest

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

    def test_live_checker_and_release_consume_the_same_qualification(self):
        # One real ledger is consumed through the CLI and release imports.
        # R1 their denominator, numerator and exact gaps agree, with no mocked
        # qualification function or hardcoded counter acting as the oracle.
        root = Path(__file__).resolve().parents[2]
        sys.path.insert(0, str(root / "scripts/release"))
        import verification_manifest
        self.assertIs(verification_manifest.profile_result, profile_result)
        features = json.loads((root / "formal/features.json").read_text())
        coverage = json.loads((root / "formal/coverage.json").read_text())
        result = profile_result(features, coverage)
        checked = json.loads(subprocess.check_output(
            [sys.executable, str(root / "scripts/ci/check_formal_coverage.py"), "--json"], text=True
        ))["verified_profile"]
        self.assertEqual(checked["required"], result["required_obligations"])
        for key in ("directly_proved_or_refined", "eligible", "gaps"):
            self.assertEqual(checked[key], result[key])


if __name__ == "__main__":
    unittest.main()
