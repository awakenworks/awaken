#!/usr/bin/env python3
"""Decision-table tests for homogeneous LLVM profile discovery."""

from __future__ import annotations

import importlib.util
import pathlib
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("export_homogeneous_lcov.py")
SPEC = importlib.util.spec_from_file_location("export_homogeneous_lcov", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ProfileGroupingTests(unittest.TestCase):
    def test_groups_only_compatible_module_signatures(self) -> None:
        # Cause graph: C1 profiles share %m; C2 profiles have a different %m;
        # E1 only C1 may enter one llvm-profdata merge, while C2 is exported as
        # another LCOV document. PID and continuous-mode suffix do not affect it.
        #
        # | Rule | PID | module signature | continuous suffix | export group |
        # |---|---:|---:|---|---|
        # | G1 | 11 | 101 | absent | 101 |
        # | G2 | 12 | 101 | present | 101 |
        # | G3 | 13 | 202 | absent | 202 |
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for name in (
                "awaken-11-101.profraw",
                "awaken-12-101-9.profraw",
                "awaken-13-202.profraw",
            ):
                (root / name).touch()

            groups = MODULE.profile_groups(root)

            self.assertEqual(sorted(groups), ["101", "202"])
            self.assertEqual(len(groups["101"]), 2)
            self.assertEqual(len(groups["202"]), 1)

    def test_rejects_unsigned_legacy_profiles(self) -> None:
        # Cause graph: C1 an old %p-only profile remains after a resumed run;
        # E1 its binary identity is unknowable, so the exporter must fail instead
        # of silently mixing it. The decision table has one safety-critical rule.
        #
        # | Rule | signed profile | legacy profile | Result |
        # |---|---|---|---|
        # | R1 | T | F | group |
        # | R2 | T | T | reject and request clean run |
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            (root / "awaken-11-101.profraw").touch()
            (root / "awaken-12.profraw").touch()

            with self.assertRaisesRegex(ValueError, "lack an LLVM module signature"):
                MODULE.profile_groups(root)


if __name__ == "__main__":
    unittest.main()
