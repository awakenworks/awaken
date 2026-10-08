"""Prove real unittest entrypoints retain full flags and reject prefixes early."""
from pathlib import Path
import subprocess
import sys
import unittest

from _strict_unittest import main


class Arguments(unittest.TestCase):
    def test_exact_flag_executes_and_unknown_or_abbreviated_flags_refuse(self):
        entry = Path(__file__).with_name("test_coverage_summary.py")
        for flag, expected in (("--locals", 0), ("--local", 2), ("--awaken-gate-argument-probe", 2)):
            with self.subTest(flag=flag):
                result = subprocess.run([sys.executable, str(entry), flag], capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                if expected:
                    self.assertIn("usage: test_coverage_summary.py", result.stderr)
                    self.assertNotIn("Ran 2 tests", result.stderr)
                else:
                    self.assertIn("Ran 2 tests", result.stderr)


if __name__ == "__main__":
    main()
