#!/usr/bin/env python3
"""Focused tests for the formal production-surface inventory boundary."""

from __future__ import annotations

import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).with_name("check_formal_surface.py")
SPEC = importlib.util.spec_from_file_location("formal_surface", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class FormalSurfaceProductionSourceTests(unittest.TestCase):
    def test_only_production_modules_enter_the_surface_denominator(self) -> None:
        # Cause/effect decision table:
        # P1 crates/*/src/lib.rs -> production surface; P2 tests.rs/test.rs,
        # *_tests.rs, or a tests/ directory -> test evidence only; P3 a Rust
        # file outside src -> not production. Effect E1: only P1 enters the
        # source-oriented denominator, so tests cannot duplicate their owner.
        for relative, expected in [
            ("crates/example/src/lib.rs", True),
            ("crates/example/src/tests.rs", False),
            ("crates/example/src/test.rs", False),
            ("crates/example/src/recovery_tests.rs", False),
            ("crates/example/src/tests/recovery.rs", False),
            ("crates/example/tests/recovery.rs", False),
        ]:
            self.assertEqual(
                MODULE.is_production_source(MODULE.ROOT / relative),
                expected,
                relative,
            )


if __name__ == "__main__":
    unittest.main()
