"""Pure Cargo target-policy controls; never build/backend evidence."""
import contextlib
import copy
import io
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest import mock

from _native_postgres_scopes import postgres_phases
import _native_work_target_coverage as policy

ROOT = Path(__file__).resolve().parents[2]


class WorkTargetMetadataTests(unittest.TestCase):
    def setUp(self):
        base = ROOT / policy.WORK_MANIFEST.parent
        names = ("archive", "environment_revision", "lease_book_properties", "work_identity",
                 "work_queue_conformance", "work_queue_parity", "work_queue_properties")
        targets = [{"name": "awaken_work_store", "kind": ["lib"], "test": True,
                    "src_path": str(base / "src/lib.rs")}]
        targets += [{"name": name, "kind": ["test"], "test": True,
                     "src_path": str(base / "tests" / (name + ".rs"))} for name in names]
        self.metadata = {"packages": [{"name": "awaken-work-store",
            "manifest_path": str(base / "Cargo.toml"), "targets": targets}]}
        self.phases = [SimpleNamespace(package=p.package, selector=p.selector, arguments=p.arguments)
                       for p in postgres_phases("core", ROOT)]

    def verify(self, metadata=None, phases=None):
        return policy.validate_work_target_coverage(
            self.metadata if metadata is None else metadata,
            self.phases if phases is None else phases, ROOT)

    def test_the_two_selectors_cover_every_original_cargo_target_once(self):
        report = self.verify()
        self.assertEqual(len(report["targets"]), 8)
        self.assertIn(["lib"], report["targets"])
        self.assertNotIn(["test", "durable"], report["targets"])

    def test_new_missing_duplicate_or_foreign_metadata_targets_fail_closed(self):
        for mutation in ("new", "missing", "duplicate", "foreign"):
            data = copy.deepcopy(self.metadata)
            targets = data["packages"][0]["targets"]
            if mutation == "new":
                target = copy.deepcopy(targets[-1])
                target["name"] = "new_required_target"
                targets.append(target)
            elif mutation == "missing":
                targets.pop()
            elif mutation == "duplicate":
                targets.append(copy.deepcopy(targets[-1]))
            else:
                targets[-1]["src_path"] = str(ROOT.parent / "foreign.rs")
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.verify(data)

    def test_foreign_missing_duplicate_packages_and_unknown_features_or_kinds_are_refused(self):
        for mutation in ("manifest", "missing", "duplicate", "feature", "kind", "test-disabled"):
            data = copy.deepcopy(self.metadata)
            package = data["packages"][0]
            if mutation == "manifest":
                package["manifest_path"] = str(ROOT.parent / "Cargo.toml")
            elif mutation == "missing":
                data["packages"] = []
            elif mutation == "duplicate":
                data["packages"].append(copy.deepcopy(package))
            elif mutation == "feature":
                package["targets"][-1]["required-features"] = ["unknown"]
            elif mutation == "kind":
                package["targets"][-1]["kind"] = ["example"]
            else:
                package["targets"][-1]["test"] = False
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.verify(data)

    def test_missing_or_duplicate_scope_and_broad_selection_cannot_qualify_coverage(self):
        for mutation in ("missing", "duplicate", "lib", "broad"):
            phases = copy.deepcopy(self.phases)
            work = [p for p in phases if p.package == "awaken-work-store"]
            if mutation == "missing":
                phases.remove(work[0])
            elif mutation == "duplicate":
                phases.append(copy.deepcopy(work[0]))
            elif mutation == "lib":
                work[1].arguments = tuple(arg for arg in work[1].arguments if arg != "--lib")
            else:
                work[1].arguments = ("--all-targets", *work[1].arguments)
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.verify(phases=phases)

    def test_each_selected_target_is_checked_against_metadata_not_a_manual_counter(self):
        for name in ("archive", "environment_revision", "lease_book_properties", "work_identity",
                     "work_queue_parity", "work_queue_properties"):
            phases = copy.deepcopy(self.phases)
            work = [p for p in phases if p.package == "awaken-work-store"][-1]
            args = list(work.arguments)
            index = args.index(name)
            del args[index - 1:index + 1]
            work.arguments = tuple(args)
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.verify(phases=phases)

    def test_metadata_error_or_invalid_json_never_becomes_a_coverage_pass(self):
        for status, output in ((75, b""), (0, b"not-json")):
            diagnostic = io.StringIO()
            with mock.patch.object(policy.subprocess, "run",
                    return_value=SimpleNamespace(returncode=status, stdout=output,
                        stderr=b"owned-metadata-refusal-detail\n")) as run:
                with contextlib.redirect_stderr(diagnostic), self.subTest(status=status), self.assertRaises(ValueError):
                    policy.read_work_metadata(ROOT)
                if status:
                    self.assertIn("owned-metadata-refusal-detail", diagnostic.getvalue())
                command = run.call_args.args[0]
                self.assertEqual(command[4:6], ["cargo", "metadata"])
                self.assertIn("--locked", command)
                self.assertIn("--offline", command)


if __name__ == "__main__":
    unittest.main(verbosity=2)
