#!/usr/bin/env python3
"""The boundary gate's next steps and `--fix` repairs, driven through the real owners.

Each case builds the owning guard's input, lets that guard refuse, applies the
repair the refusal names, and asks the same guard again: a mechanical refusal
must then pass, and a real violation beside it must still be refused.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import _bounded_context_fitness  # noqa: E402
import _context_map_fitness  # noqa: E402
import _crate_boundary_fix as fix  # noqa: E402
import _dependency_use_fitness  # noqa: E402
import _ownership_fence_fitness  # noqa: E402
import _strict_unittest as strict_unittest  # noqa: E402
import check_crate_boundaries  # noqa: E402
import formal_sources  # noqa: E402

class BaselineRepairTests(unittest.TestCase):
    def scripts(self, directory: str, name: str, document: dict) -> Path:
        path = Path(directory) / name
        path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
        return path

    def test_a_stale_crossing_is_removed_and_a_new_crossing_stays_refused(self) -> None:
        rule = "context-contract-seam"
        found = {name: set() for name in ("sdk-standalone", "extension-engine-split", rule, "protocol-leaf",
                                          "composition-root-leaf")}
        found[rule] = {("held", "kept"), ("fresh", "crossing")}
        baseline = {rule: {"held -> kept": "planned", "gone -> away": "planned"}}
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(fix, "SCRIPTS", Path(directory)):
            path = self.scripts(directory, "bounded_context_baseline.json", baseline)
            errors = _bounded_context_fitness.ratchet_errors(found, baseline)
            self.assertIn(f"{rule}: gone -> away no longer violates; remove it from bounded_context_baseline.json "
                          "in this commit", errors)
            done, refused = fix.apply(errors)
            self.assertEqual((done, refused), ([f"removed {rule} gone -> away from bounded_context_baseline.json"], []))
            repaired = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(repaired, {rule: {"held -> kept": "planned"}})
            after = _bounded_context_fitness.ratchet_errors(found, repaired)
            self.assertEqual(len(after), 1)
            self.assertTrue(after[0].startswith(f"{rule}: new dependency fresh -> crossing"), after)
            self.assertIsNone(fix.repair_for(after[0]))

    def test_a_fallen_spi_count_is_lowered_and_a_risen_one_stays_refused(self) -> None:
        baseline = {"spi-paths": {"a": {"x::y": 3, "x::z": 1}, "b": {"x::y": 1}},
                    "crate-dependencies": {"a -> engine": "planned", "b -> engine": "planned"}}
        paths = {"a": {"x::y": 2}, "b": {"x::y": 2}}
        crates = {("a", "engine")}
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(fix, "SCRIPTS", Path(directory)):
            path = self.scripts(directory, "sdk_engine_spi_baseline.json", baseline)
            errors = _context_map_fitness.spi_ratchet_errors(paths, crates, baseline)
            done, refused = fix.apply(errors)
            self.assertEqual(refused, [])
            self.assertEqual(len(done), 3, done)
            repaired = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(repaired, {"spi-paths": {"a": {"x::y": 2}, "b": {"x::y": 1}},
                                        "crate-dependencies": {"a -> engine": "planned"}})
            after = _context_map_fitness.spi_ratchet_errors(paths, crates, repaired)
            self.assertEqual(len(after), 1)
            self.assertTrue(after[0].startswith("sdk-engine-spi: b names x::y at 2 site(s), baseline 1"), after)
            # A refusal that no longer matches the file cannot raise a count.
            done, refused = fix.apply(["sdk-engine-spi: b names x::y at 5 site(s), below the baseline 6; lower"])
            self.assertEqual(done, [])
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), repaired)

    def test_a_ratchet_below_its_baseline_runs_that_guards_tighten_and_a_refusing_one_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(fix, "SCRIPTS", Path(directory)):
            scripts = Path(directory)
            (scripts / "_lower_fitness.py").write_text(
                "import sys, pathlib\nassert sys.argv[1:] == ['--tighten']\n"
                "pathlib.Path(__file__).with_name('lowered').write_text('1')\nprint('wrote lower.json')\n")
            (scripts / "_grown_fitness.py").write_text(
                "import sys\nprint('ERROR: x 1 -> 2; fix: split', file=sys.stderr)\nsys.exit(1)\n")
            errors = [f"r: k 2 -> 1; fix: python3 scripts/ci/_{name}_fitness.py --tighten to lower {name}.json "
                      "in this commit" for name in ("lower", "grown")]
            done, refused = fix.apply(errors, Path(directory))
            self.assertEqual(done, ["_lower_fitness.py --tighten: wrote lower.json"])
            self.assertTrue((scripts / "lowered").exists())
            self.assertEqual(len(refused), 1)
            self.assertIn("_grown_fitness.py --tighten refused: ERROR: x 1 -> 2; fix: split", refused[0])

    def test_the_model_binding_constant_is_only_lowered(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            checker = Path(directory) / "check.py"
            checker.write_text("A = 1\nUNVALIDATED_MODEL_BINDING_BASELINE = 405\nB = 2\n", encoding="utf-8")
            below = "403 ModelBinding::new call sites, below the baseline of 405. Lower ..."
            above = "406 ModelBinding::new call sites, baseline 405: 1 new producer(s) use panic-based admission."
            self.assertIsNone(fix.repair_for(above))
            self.assertEqual(fix.apply([below, above], checker=checker),
                             (["lowered UNVALIDATED_MODEL_BINDING_BASELINE 405 -> 403"], []))
            self.assertEqual(checker.read_text(encoding="utf-8"),
                             "A = 1\nUNVALIDATED_MODEL_BINDING_BASELINE = 403\nB = 2\n")
            # A stale refusal cannot raise the constant back.
            self.assertEqual(fix.apply(["404 ModelBinding::new call sites, below the baseline of 405."],
                                       checker=checker)[0], [])


class DependencyRepairTests(unittest.TestCase):
    def workspace(self, directory: str, manifest: str) -> Path:
        root = Path(directory)
        for name, text in (("a", manifest), ("b", '[package]\nname = "b"\n')):
            crate = root / "crates" / name
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(text, encoding="utf-8")
        (root / "crates/b/src/lib.rs").write_text("pub fn b() {}\n", encoding="utf-8")
        (root / "crates/a/src/lib.rs").write_text(
            "pub fn a() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { b::b(); }\n}\n", encoding="utf-8")
        return root

    def errors(self, root: Path) -> list[str]:
        members = frozenset({"a", "b"})
        uses, enables = _dependency_use_fitness.crate_uses(root / "crates/a/Cargo.toml", members)
        return _dependency_use_fitness.dependency_use_errors(uses, {"a": enables} if enables else {})

    def test_a_test_only_dependency_moves_and_the_guard_then_passes(self) -> None:
        manifest = ('[package]\nname = "a"\n\n[dependencies]\nb = { path = "../b" }\n\n'
                    '[dev-dependencies]\n')
        with tempfile.TemporaryDirectory() as directory:
            root = self.workspace(directory, manifest)
            errors = self.errors(root)
            self.assertEqual(errors, ["dependency-use: a names b only in test code; move it to [dev-dependencies]"])
            self.assertIn("next: `python3 scripts/ci/check_crate_boundaries.py --fix` moves it in crates/a/Cargo.toml",
                          fix.annotate(errors, root)[0])
            done, refused = fix.apply(errors, root)
            self.assertEqual((done, refused), (["moved b to [dev-dependencies] in crates/a/Cargo.toml"], []))
            self.assertEqual((root / "crates/a/Cargo.toml").read_text(encoding="utf-8"),
                             '[package]\nname = "a"\n\n[dependencies]\n\n[dev-dependencies]\nb = { path = "../b" }\n')
            self.assertEqual(self.errors(root), [])

    def test_an_optional_or_feature_named_dependency_is_left_for_a_person(self) -> None:
        for manifest in ('[package]\nname = "a"\n\n[dependencies]\nb = { path = "../b", optional = true }\n',
                         '[package]\nname = "a"\n\n[dependencies]\nb = { path = "../b" }\n\n[features]\nx = ["b/y"]\n'):
            with self.subTest(manifest=manifest), tempfile.TemporaryDirectory() as directory:
                root = self.workspace(directory, manifest)
                done, refused = fix.apply(["dependency-use: a names b only in test code; move it to "
                                           "[dev-dependencies]"], root)
                self.assertEqual(done, [])
                self.assertTrue(refused and "decide by hand" in refused[0], refused)
                self.assertEqual((root / "crates/a/Cargo.toml").read_text(encoding="utf-8"), manifest)


class NextStepTests(unittest.TestCase):
    def test_every_historically_bare_refusal_gains_a_next_step(self) -> None:
        cases = {
            "unlisted Awaken managed extension route: POST /v1/awaken/sessions/{}/runs/{}/cancel":
                'add ("POST", "/v1/awaken/sessions/{}/runs/{}/cancel") to AWAKEN_MANAGED_EXTENSION_ROUTES in '
                "scripts/ci/_managed_protocol_boundary.py",
            "unregistered public route owner source crates/x/src/routes.rs; add it to ROUTE_OWNER_FILES":
                'add "crates/x/src/routes.rs" to ROUTE_OWNER_FILES in scripts/ci/_execution_ownership_fitness.py',
            "crate-separation: awaken-sqlite-pool isolates 'sqlx-core', which is not in ISOLATED_DEPENDENCIES":
                "to ISOLATED_DEPENDENCIES in scripts/ci/_crate_separation_fitness.py",
            "crates/x/src/sqlite/admission_tests.rs: production DDL is not owned by a versioned Migration":
                "move the DDL in crates/x/src/sqlite/admission_tests.rs into a versioned migration",
        }
        for error, step in cases.items():
            with self.subTest(error=error):
                annotated = fix.annotate([error])[0]
                self.assertTrue(annotated.startswith(error + "; next: "), annotated)
                self.assertIn(step, annotated)
        # A refusal that already names its fix is left as its owner wrote it.
        owned = "naming: turn-noun: a::turn 0 -> 1 (x.rs:3); fix: use Thread, Run or Step"
        self.assertEqual(fix.annotate([owned]), [owned])

    def test_the_summary_counts_only_mechanical_refusals_as_fixable(self) -> None:
        errors = ["context-contract-seam: a -> b no longer violates; remove it from bounded_context_baseline.json",
                  "naming: turn-noun: a::turn 0 -> 1 (x.rs:3); fix: use Thread, Run or Step"]
        text = fix.summary(errors)
        self.assertIn("2 refusal(s); 1 of them are mechanical: run `python3 scripts/ci/check_crate_boundaries.py "
                      "--fix`", text)
        self.assertIn("the other 1 need the change named after `fix:`/`next:`", text)
        self.assertNotIn("--fix", fix.summary(errors[1:]))

    def test_fence_count_refusals_name_the_lines_that_grew(self) -> None:
        errors = _ownership_fence_fitness.ownership_fence_violations(
            {"crates/a.rs": "fn f(e: u8) {}"},
            {"crates/a.rs": "fn f(e: u8) {}\nfn g(next_epoch: u64) -> u64 { next_epoch + 1 }"})
        self.assertTrue(any(e.startswith("F2 ") and e.endswith("(at crates/a.rs:2)") for e in errors), errors)
        self.assertTrue(any(e.startswith("F3 ") and e.endswith("(at crates/a.rs:2)") for e in errors), errors)


class ModelBindingSiteTests(unittest.TestCase):
    def git(self, root: Path, *arguments: str) -> None:
        subprocess.run(["git", "-c", "user.name=fixture", "-c", "user.email=fixture@invalid", *arguments],
                       cwd=root, env=formal_sources.detached_git_environment(), check=True, capture_output=True)

    def test_a_count_above_the_baseline_names_the_sites_added_since_head(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/x/src/lib.rs"
            other = root / "crates/y/src/lib.rs"
            for path in (source, other):
                path.parent.mkdir(parents=True)
                path.write_text("fn held() { let _ = ModelBinding::new(a, b, c); }\n", encoding="utf-8")
            self.git(root, "init", "--quiet")
            self.git(root, "add", "--all")
            self.git(root, "commit", "--quiet", "-m", "held")
            source.write_text(source.read_text(encoding="utf-8")
                              + "fn added() {\n    let _ = ModelBinding::new(a, b, c);\n}\n", encoding="utf-8")
            with mock.patch.object(check_crate_boundaries, "UNVALIDATED_MODEL_BINDING_BASELINE", 2):
                errors = check_crate_boundaries.check_model_binding_producers_are_ratcheted(root)
            self.assertEqual(len(errors), 1)
            self.assertIn("Sites in the files whose count grew since HEAD: crates/x/src/lib.rs:1, "
                          "crates/x/src/lib.rs:3.", errors[0])
            self.assertNotIn("crates/y", errors[0])
            self.assertIn("fix: write `ModelBinding::parse(..)` at each new site", errors[0])


if __name__ == "__main__":
    strict_unittest.main()
