"""Coverage anchors survive line shifts, never a changed or ambiguous check."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import check_session_acceptance as owner


class SourceAnchorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="coverage-source-anchor-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.data = owner._fixture(self.root)
        self.original = (self.root / "src/lib.rs").read_text(encoding="utf-8")
        ledger = self.root / owner.LEDGER
        ledger.mkdir(parents=True)
        parts = []
        for key in ("groups", "bindings", "evidence_registry"):
            name = key + ".json"
            (ledger / name).write_text(json.dumps(self.data[key]), encoding="utf-8")
            parts.append({"key": key, "file": name})
        (ledger / "catalog.json").write_text(
            json.dumps({**self.data["catalog"], "parts": parts}), encoding="utf-8")
        self.environment = {key: value for key, value in os.environ.items()
                            if not key.startswith("GIT_")}
        self.environment.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        self.git("init", "--quiet")
        self.commit()

    def git(self, *arguments: str) -> bytes:
        return subprocess.check_output(
            ["git", "-c", "user.name=Coverage fixture", "-c",
             "user.email=coverage@example.invalid", *arguments],
            cwd=self.root, env=self.environment, stderr=subprocess.PIPE)

    def commit(self) -> None:
        self.git("add", "--", "src", owner.LEDGER.as_posix())
        self.git("commit", "--quiet", "-m", "test: retain source anchor fixture")

    def write(self, source: str) -> None:
        (self.root / "src/lib.rs").write_text(source, encoding="utf-8")

    def errors(self, data: dict | None = None) -> list[str]:
        return owner.ledger_errors(data or self.data, self.root,
                                   owner.committed_registry(self.root))

    def test_line_shift_preserves_exact_checks_without_editing_the_coverage_index(self) -> None:
        self.assertEqual(self.errors(), [])
        index = self.root / owner.LEDGER / "bindings.json"
        before = index.read_bytes()
        self.write("// A source comment.\n// Another source comment.\n\n" + self.original)
        self.assertEqual(self.errors(), [])
        self.assertEqual(index.read_bytes(), before)
        self.commit()
        # HEAD now contains the shift. The unchanged binding's own commit,
        # rather than the current source at its old line, remains the anchor.
        self.assertEqual(self.errors(), [])
        self.assertEqual(index.read_bytes(), before)

    def test_replaced_assertion_cannot_borrow_an_assertion_at_the_same_line(self) -> None:
        self.write(self.original.replace("assert_eq!(x, 1);", "assert!(false);"))
        self.assertTrue(any(error.startswith("L04 B900") for error in self.errors()), self.errors())

    def test_deleted_check_cannot_borrow_a_matching_line_from_another_function(self) -> None:
        other = "fn unrelated() {\n    let x = 1;\n    assert_eq!(x, 1);\n}\n"
        self.write(other + self.original.replace("    assert_eq!(x, 1);\n", ""))
        self.assertTrue(any(error.startswith("L04 B900") for error in self.errors()), self.errors())

    def test_a_copied_body_cannot_transfer_its_check_to_a_different_function(self) -> None:
        original = self.original.replace("    let x = 1;", "    let before = 0;\n    let x = 1;\n    let after = 2;")
        self.write(original)
        self.data["bindings"][0]["assertion_locations"][0]["line"] = 5
        ledger = self.root / owner.LEDGER / "bindings.json"
        ledger.write_text(json.dumps(self.data["bindings"]), encoding="utf-8")
        self.commit()
        copied = original[:original.index("fn named_only")].replace("fn real_case()", "fn unrelated()")
        self.write(copied + original.replace("    assert_eq!(x, 1);\n", ""))
        self.assertTrue(any(error.startswith("L04 B900") for error in self.errors()), self.errors())

    def test_a_duplicate_source_block_is_ambiguous_instead_of_an_anchor(self) -> None:
        self.write(self.original + self.original)
        self.assertTrue(any(error.startswith("L04 B900") for error in self.errors()), self.errors())

    def test_renamed_or_moved_test_still_requires_the_declared_owner(self) -> None:
        self.write(self.original.replace("fn real_case()", "fn renamed_case()"))
        self.assertTrue(any(error.startswith("L03 B900") for error in self.errors()), self.errors())
        self.write(self.original)
        (self.root / "src/lib.rs").rename(self.root / "src/moved.rs")
        self.assertTrue(any(error.startswith("L02 B900") for error in self.errors()), self.errors())

    def test_uncommitted_forged_coordinates_cannot_use_the_registered_anchor(self) -> None:
        forged = copy.deepcopy(self.data)
        forged["bindings"][0]["assertion_locations"][0]["line"] = 6
        self.assertTrue(any(error.startswith("L04 B900") for error in self.errors(forged)))

    def test_foreign_git_process_variables_cannot_redirect_anchor_reads(self) -> None:
        self.write("// A source comment.\n\n" + self.original)
        with patch.dict(os.environ, {"GIT_DIR": str(self.root / "absent.git"),
                                     "GIT_WORK_TREE": str(self.root / "absent-worktree"),
                                     "GIT_INDEX_FILE": str(self.root / "absent.index")}):
            # Only location resolution is under test: the registry is frozen
            # before the deliberately foreign process variables are injected.
            frozen = {entry["id"]: owner.entry_digest(entry)
                      for entry in self.data["evidence_registry"]}
            self.assertEqual(owner.ledger_errors(self.data, self.root, frozen), [])

    def test_a_corrupted_base_body_cannot_replace_its_registered_digest(self) -> None:
        frozen = owner.committed_registry(self.root)
        corrupt = copy.deepcopy(self.data["evidence_registry"])
        corrupt[0]["receipt"] = "target/incorrect-location.json"
        registry = self.root / owner.LEDGER / "evidence_registry.json"
        registry.write_text(json.dumps(corrupt), encoding="utf-8")
        self.commit()
        self.assertEqual(owner.committed_registry(self.root), frozen)
        self.assertEqual(self.errors(), [])
        loaded = owner._load(self.root)
        self.assertTrue(any(error.startswith("L11 E90") for error in self.errors(loaded)))
        changed = copy.deepcopy(self.data)
        changed["evidence_registry"][0]["receipt"] = "target/another-location.json"
        self.assertTrue(any(error.startswith("L11 E90") for error in self.errors(changed)))

    def test_the_committed_snapshot_freezes_secondary_registry_parts(self) -> None:
        entry = copy.deepcopy(self.data["evidence_registry"][0])
        entry["id"] = "E91"
        entry["entry_sha256"] = owner.entry_digest(entry)
        ledger = self.root / owner.LEDGER
        (ledger / "secondary.json").write_text(json.dumps([entry]), encoding="utf-8")
        catalog = json.loads((ledger / "catalog.json").read_text(encoding="utf-8"))
        catalog["parts"].append({"key": "evidence_registry", "file": "secondary.json"})
        (ledger / "catalog.json").write_text(json.dumps(catalog), encoding="utf-8")
        self.commit()
        frozen = owner.committed_registry(self.root)
        self.assertEqual(set(frozen), {"E90", "E91"})
        changed = copy.deepcopy(self.data)
        changed["evidence_registry"].append(copy.deepcopy(entry))
        self.assertEqual(self.errors(changed), [])
        changed["evidence_registry"][-1]["receipt"] = "target/another-location.json"
        self.assertTrue(any(error.startswith("L11 E91") for error in self.errors(changed)))

    def test_new_unregistered_sources_keep_the_original_negative_controls(self) -> None:
        self.assertEqual(owner.self_test(), 0)


if __name__ == "__main__":
    unittest.main()
