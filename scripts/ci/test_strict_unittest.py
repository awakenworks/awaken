"""Prove real unittest entrypoints retain full flags and reject prefixes early."""
from pathlib import Path
import io
import json
import os
import subprocess
import sys
import unittest
from unittest.mock import patch

from _strict_unittest import main, observed_runner

# The subprocess exercises this real entrypoint. Other walked paths are
# private fixture observations; imported helpers use the shared input closure.
INPUT_GLOBS = ('scripts/ci/test_coverage_summary.py',)


class Arguments(unittest.TestCase):
    def test_case_timings_keep_failure_skip_and_expected_failure_distinct(self):
        # Causes: success, assertion/error/subtest failure, skip, expected and
        # unexpected failure; active/missing/foreign task context. Effects:
        # original unittest verdict is unchanged; one finite case row per
        # completed case, exact semantic status, own run identity. Missing
        # context writes nothing; a foreign context refuses instead of appending.
        from _build_observation_fixture import real_build_observations, synthetic_build_environment
        from ci_performance import record_stage
        class Cases(unittest.TestCase):
            def test_pass(self): pass
            def test_fail(self): self.fail('expected probe')
            def test_error(self): raise ValueError('expected probe')
            def test_subtest(self):
                with self.subTest(row=1): self.fail('expected probe')
            @unittest.skip('probe skip')
            def test_skip(self): pass
            @unittest.expectedFailure
            def test_expected(self): self.fail('expected probe')
            @unittest.expectedFailure
            def test_unexpected(self): pass
        task, root, _ = real_build_observations(self)
        with patch.dict(os.environ, task.environment, clear=True):
            result = observed_runner(stream=io.StringIO()).run(unittest.defaultTestLoader.loadTestsFromTestCase(Cases))
        self.assertFalse(result.wasSuccessful())
        rows = [row for path in (task.directory / 'commands').glob('*.json')
                for row in json.loads(path.read_text())['rows']]
        self.assertEqual(len(rows), 7)
        expected = dict(test_pass='passed', test_skip='skipped', test_expected='expected-failure')
        for row in rows:
            outcome = expected.get(row['label'].rsplit('.', 1)[1], 'failed')
            self.assertEqual(row['semantic_result'], outcome)
            self.assertEqual(row['exit_code'], int(outcome == 'failed'))
            self.assertGreaterEqual(row['duration_seconds'], 0)
        with patch.dict(os.environ, {}, clear=True):
            record_stage('no-context', 0, 0)
            for invalid in (-1, float('nan'), float('inf')):
                with self.assertRaises(ValueError): record_stage('invalid', invalid, 0)
        with patch.dict(os.environ, {**task.environment, 'AWAKEN_BUILD_RUN_ID': 'foreign'}, clear=True):
            with self.assertRaises(ValueError): record_stage('foreign', 0, 0)
        with patch.dict(os.environ, {**task.environment, 'AWAKEN_BUILD_INVOCATION_ID': 'parent'}, clear=True):
            synthetic = synthetic_build_environment(root)
        self.assertNotIn('AWAKEN_BUILD_RUN_ID', synthetic)
        self.assertNotIn('AWAKEN_BUILD_INVOCATION_ID', synthetic)
        self.assertEqual(synthetic['AWAKEN_BUILD_OBSERVATION_KIND'], 'synthetic')
        self.assertEqual(synthetic['AWAKEN_BUILD_OBSERVATIONS'], str(root / 'target/observations'))
        from ci_performance import write_report
        output = root.parent / 'case-costs.json'
        with patch.dict(os.environ, {}, clear=True):
            write_report({'local': {'commands': rows}}, output)
        self.assertIn('skipped', output.with_suffix('.md').read_text())
        self.assertIn('not additional wall time', output.with_suffix('.md').read_text())
        task.finish(1)

    def test_exact_flag_executes_and_unknown_or_abbreviated_flags_refuse(self):
        # A1 exact flag executes the entrypoint's complete nonempty inventory;
        # A2 abbreviated/unknown flags exit 2 before any test summary. Derive
        # the inventory through unittest instead of freezing another suite's
        # case count (which hid execution whenever that suite grew).
        import test_coverage_summary
        count = unittest.defaultTestLoader.loadTestsFromModule(test_coverage_summary).countTestCases()
        self.assertGreater(count, 0)
        entry = Path(__file__).with_name("test_coverage_summary.py")
        for flag, expected in (("--locals", 0), ("--local", 2), ("--awaken-gate-argument-probe", 2)):
            with self.subTest(flag=flag):
                result = subprocess.run([sys.executable, str(entry), flag], capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                if expected:
                    self.assertIn("usage: test_coverage_summary.py", result.stderr)
                    self.assertNotRegex(result.stderr, r"Ran \d+ tests?")
                else:
                    self.assertIn(f"Ran {count} tests", result.stderr)


if __name__ == "__main__":
    main()
