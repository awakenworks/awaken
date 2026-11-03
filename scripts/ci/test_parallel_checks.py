#!/usr/bin/env python3
"""Decision-table tests for the static queue and its real deadline worker."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from _strict_unittest import main

ROOT = Path(__file__).resolve().parents[2]


@unittest.skipIf(os.name == "nt", "canonical Bash gate runs in Linux")
class ParallelTests(unittest.TestCase):
    def run_queue(self, root: Path, body: str, jobs: int = 2) -> subprocess.CompletedProcess[str]:
        script = f'''set -u
source {shlex.quote(str(ROOT / 'scripts/ci/_parallel_checks.sh'))}
parallel_checks_init {jobs} {shlex.quote(str(root / 'evidence'))} || exit $?
parallel_checks_result() {{ printf '%s|%s\\n' "$1" "$2" >> {shlex.quote(str(root / 'results'))}; }}
{body}
parallel_checks_drain
'''
        return subprocess.run(["bash", "-c", script], cwd=root, capture_output=True, text=True, timeout=20)

    def test_parallel_barrier_and_complete_failure_aggregation(self) -> None:
        # Q1 two independently submitted checks must both reach the barrier;
        # Q2 a later failed job retains its exact exit code; Q3 all jobs drain.
        # A sequential replacement cannot satisfy the first positive control.
        # SX5: the generated child carries the shared wait spelling in its
        # Python comment. Its raw fixture string changes; the barrier, deadline,
        # child code and observed queue results remain these existing oracles.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "barrier.py").write_text('''import pathlib,sys,time
p=pathlib.Path(sys.argv[1]); p.write_text('ready')
other=pathlib.Path(sys.argv[2]); until=time.monotonic()+5
# awaken-allow(wait): both actual child processes acknowledge the shared barrier.
while not other.exists() and time.monotonic()<until: time.sleep(0.01)
assert other.exists(), 'both jobs must run concurrently'
''')
            result = self.run_queue(root, """parallel_checks_submit first python3 barrier.py first second
parallel_checks_submit second python3 barrier.py second first
parallel_checks_submit failure sh -c 'exit 23'
""")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertCountEqual((root / "results").read_text().splitlines(), ["first|0", "second|0", "failure|23"])
            self.assertTrue(all((root / "evidence" / f"{index}.json").is_file() for index in range(3)))

    def test_finished_worker_frees_capacity_while_oldest_is_still_running(self) -> None:
        # SX5: the child program's wait annotation is source metadata; the
        # actual third-worker fact still owns this capacity-release assertion.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "wait_for_third.py").write_text('''import pathlib,time
until=time.monotonic()+5
# awaken-allow(wait): third is the actual next admitted worker's completion signal.
while not pathlib.Path('third').exists() and time.monotonic()<until: time.sleep(0.01)
assert pathlib.Path('third').exists(), 'a completed second worker must free its slot'
''')
            result = self.run_queue(root, """parallel_checks_submit oldest python3 wait_for_third.py
parallel_checks_submit second true
parallel_checks_submit third touch third
""")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertCountEqual((root / "results").read_text().splitlines(), ["oldest|0", "second|0", "third|0"])

    def test_missing_or_corrupt_receipt_cannot_become_green(self) -> None:
        for replacement in ("rm evidence/0.json", "printf '{}' > evidence/0.json"):
            with self.subTest(replacement=replacement), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                result = self.run_queue(root, f"parallel_checks_submit observed true\nwait \"${{parallel_pids[0]}}\"\n{replacement}\n")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root / "results").read_text().strip(), "observed|1")

    def test_capacity_is_not_silently_unbounded(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            # With one slot, submit(second) must observe and retire first before
            # second can run. The child observes the real parent callback fact.
            result = self.run_queue(root, """parallel_checks_submit first true
parallel_checks_submit second sh -c 'test -s results'
test -s results || exit 55
""", jobs=1)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((root / "results").read_text().splitlines(), ["first|0", "second|0"])
        for jobs in (0, 33):
            with tempfile.TemporaryDirectory() as temporary:
                self.assertEqual(self.run_queue(Path(temporary), "", jobs=jobs).returncode, 2)

    def test_worker_timeout_and_signal_are_failures_with_child_cleanup(self) -> None:
        from test_deadline import cleanup_fixture_group

        for signal_worker in (False, True):
            with self.subTest(signal_worker=signal_worker), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                marker = root / "pid"
                receipt = root / "receipt.json"
                child = ("import os,pathlib,time,sys;p=pathlib.Path(sys.argv[1]);"
                         "pending=p.with_suffix('.pending');pending.write_text(str(os.getpid()));"
                         "pending.replace(p);time.sleep(120)")
                process = subprocess.Popen([sys.executable, str(ROOT / "scripts/ci/parallel_check.py"), "--receipt", str(receipt), "--timeout", "1", "--", sys.executable, "-c", child, str(marker)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    until = time.monotonic() + 5
                    # awaken-allow(wait): the actual child PID exists before cancellation.
                    while not marker.exists() and process.poll() is None and time.monotonic() < until:
                        time.sleep(0.01)
                    self.assertTrue(marker.exists())
                    if signal_worker:
                        process.terminate()
                    output = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 143 if signal_worker else 124, output)
                    self.assertNotEqual(json.loads(receipt.read_text())["status"], 0)
                    with self.assertRaises(ProcessLookupError):
                        os.kill(int(marker.read_text()), 0)
                finally:
                    cleanup_fixture_group(marker, process)

    def test_resource_refusal_stops_dependent_gate_work_with_exact_evidence(self):
        # G1 a selected sequential gate returns 75 => preserve 75 and its
        # failure kind; zero later browser execution and no full receipt.
        # G2 full-gate host observation refuses before admission => 75, zero
        # Cargo/body calls. G3 the Rust full artifact/host preflight refuses ->
        # 75 survives both entrypoints and starts no build. These run the actual
        # manifest over owned stubs;
        # they prove failure composition, not passing product qualification.
        # G3 has two real manifest stages: an admitted prior runner self-test,
        # then resource refusal75. The fixture used to omit test.sh, yielding
        # failed127 before failed75. Supply only that explicit prerequisite
        # port; it is not evidence that the real crate runner's tests passed.
        # Unknown/ordinary argv cannot use the stub as a passing runner.
        for mode in ('frontend', 'full', 'rust'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                scripts = root / 'scripts/ci'; scripts.mkdir(parents=True)
                for name in ('check-all.sh', '_provider_environment.sh', '_parallel_checks.sh',
                             'parallel_check.py', 'deadline.py', '_deadline.sh',
                             'check-rust.sh', '_cargo_target.sh', '_rust_test_execution.sh',
                             '_build_observation.sh', 'ci_performance.py',
                             'formal_run_evidence.py', 'formal_sources.py', 'formal_plan.py',
                             'formal_anchors.py', 'formal_kani_options.py',
                             '_crate_boundary_workspace.py', '_crate_dependency_fitness.py'):
                    shutil.copyfile(ROOT / 'scripts/ci' / name, scripts / name)
                (scripts / 'check-rust.sh').chmod(0o755)
                (root / 'Cargo.toml').write_text('[workspace]\nmembers=[]\n')
                tools = scripts / 'gate_tools.py'
                tools.write_text('import sys\nraise SystemExit(75 if any(x in sys.argv for x in ("resources", "disk")) else 0)\n')
                gate = scripts / 'check-frontend.sh'
                gate.write_text('#!/bin/sh\necho fixture-resource-refusal >&2\nexit 75\n')
                gate.chmod(0o755)
                binary = root / 'bin'; binary.mkdir()
                for name, body in {
                        'git': '#!/bin/sh\nprintf "%040d\\n" 1\n',
                        'pnpm': '#!/bin/sh\ntouch "$FORBIDDEN_BODY"\n',
                        'cargo': '#!/bin/sh\ntouch "$FORBIDDEN_BODY"\n'}.items():
                    path = binary / name; path.write_text(body); path.chmod(0o755)
                env = dict(os.environ, PATH=str(binary)+os.pathsep+os.environ['PATH'],
                           FORBIDDEN_BODY=str(root/'unexpected'),
                           AWAKEN_TEST_TIMINGS_FILE=str(root/'timings.tsv'))
                for name in ('AWAKEN_CARGO_ADMITTED', 'AWAKEN_FULL_GATE_CANDIDATE', 'AWAKEN_FULL_GATE_LOG'):
                    env.pop(name, None)
                prior = scripts / 'test.sh'
                prior.write_text('#!/bin/sh\nif [ "$#" -eq 1 ] && [ "$1" = --self-test ]; then exit 0; fi\ntouch "$FORBIDDEN_BODY"\nexit 2\n')
                prior.chmod(0o755)
                for arguments in ([], ['--unknown'], ['--selftest'], ['-p', 'fixture']):
                    refused = subprocess.run([str(prior), *arguments], env=env,
                                             capture_output=True, text=True, timeout=5)
                    self.assertEqual(refused.returncode, 2)
                    self.assertTrue((root/'unexpected').exists())
                    (root/'unexpected').unlink()
                args = ['--full-gate', '--record'] if mode == 'full' else ['--group', mode]
                result = subprocess.run(['bash', str(scripts/'check-all.sh'), *args],
                                        env=env, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 75, result.stdout+result.stderr)
                self.assertFalse((root/'unexpected').exists())
                self.assertFalse((scripts/'gate_record.json').exists())
                if mode != 'full':
                    import csv
                    with (root/'timings.tsv').open() as stream:
                        rows = list(csv.DictReader(stream, delimiter='\t'))
                    if mode == 'rust':
                        self.assertEqual(len(rows), 2)
                        self.assertEqual((rows[0]['check'], rows[0]['status'], rows[0]['exit_code'], rows[0]['failure_kind']),
                                         ('crate test runner self-test', 'passed', '0', 'none'))
                        self.assertEqual((rows[1]['check'], rows[1]['status'], rows[1]['exit_code'], rows[1]['failure_kind']),
                                         ('rust', 'failed', '75', 'resource-unavailable'))
                    else:
                        self.assertEqual(len(rows), 1)
                        self.assertEqual((rows[0]['exit_code'], rows[0]['failure_kind']),
                                         ('75', 'resource-unavailable'))


if __name__ == "__main__":
    main()
