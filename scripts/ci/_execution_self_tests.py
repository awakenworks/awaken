"""Bound Windows execution fixtures by processes under one enclosing deadline."""
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from threading import Event, Lock
import unittest

WORKERS = 3
SHARED_LAUNCHER = 'AWAKEN_EXECUTION_SELF_TEST_LAUNCHER'

# Schedule the longest native Git fixtures first. Independent outcomes are
# discovered separately so the same workers can distribute their actual work.
EARLY_CASES = (
    'test_build_fact_never_qualifies_execution_or_another_subject',
    'test_mutations_at_each_sequence_fence_stop_or_reject_execution',
    'test_required_source_honors_same_hook_index',
    'test_required_source_detaches_foreign_hook',
    'test_build_failure_source_drift_and_writer_fault_keep_first_cause',
    'test_prepared_graph_refuses_compile_environment_config_and_tool_drift',
    'test_required_runner_accepts_success',
    'test_required_runner_rejects_empty_selection',
    'test_required_runner_rejects_ignored_case',
    'test_required_runner_rejects_fixture_skip',
    'test_required_runner_rejects_legacy_fixture_skip',
    'test_required_runner_rejects_execution_failure',
    'test_required_runner_keeps_listing_failure',
)


def validate_report(data, case, source, raw_exit):
    if (not isinstance(data, dict) or raw_exit != 0 or data.get('schema_version') != 1
            or data.get('case') != case or data.get('source') != source
            or type(data.get('tests_run')) is not int
            or data['tests_run'] != 1 or data.get('successful') is not True
            or data.get('outcome') not in ('passed', 'skipped', 'expected-failure')):
        raise ValueError('self-test worker lacks its exact successful case/source/terminal')
    if any(data.get(key) != [] for key in ('failures', 'errors', 'unexpected_successes')):
        raise ValueError('self-test worker reports a failed test or subtest')
    skipped = data.get('skipped')
    if (not isinstance(skipped, list) or (data['outcome'] == 'skipped' and not skipped)
            or (data['outcome'] == 'passed' and skipped)
            or any(not isinstance(row, list) or len(row) != 2 or not isinstance(row[0], str)
                   or not (row[0] == case or row[0].startswith((case + ' (', case + ' [')))
                   or not isinstance(row[1], str) or not row[1] for row in skipped)):
        raise ValueError('self-test worker skip identity/reason is malformed')
    expected = data.get('expected_failures')
    if (not isinstance(expected, list) or any(name != case for name in expected)
            or (data['outcome'] == 'expected-failure') != bool(expected)):
        raise ValueError('self-test worker expected-failure identity is malformed')
    return data


def source_identity(script):
    return {'script_sha256': hashlib.sha256(script.read_bytes()).hexdigest(),
            'worker_helper_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'launcher_source_sha256': hashlib.sha256(
                Path(__file__).with_name('_native_test_launcher.py').read_bytes()).hexdigest(),
            'worker_fixture_sha256': hashlib.sha256(
                Path(__file__).with_name('_execution_self_test_fixtures.py').read_bytes()).hexdigest()}


def run_case(script, case, report, *, admitted=False):
    from _strict_unittest import observed_runner
    source = source_identity(script)
    if admitted:
        from formal_run_evidence import write_record
        ready = {'case': case.id(), 'source': source, 'pid': os.getpid()}
        write_record(report.with_suffix('.ready'), ready)
        sys.stdout.buffer.write(json.dumps(ready).encode('utf-8') + b'\n')
        sys.stdout.buffer.flush()
        # The deadline trampoline has joined the parent's Job. EOF means the
        # parent cancelled or died; neither setup nor the case may start.
        if sys.stdin.buffer.read(1) != b'\x01':
            return 1
        if source_identity(script) != source:
            raise ValueError('self-test source changed before its admission')
        if descriptor := os.environ.pop(SHARED_LAUNCHER, None):
            from _native_test_launcher import use_shared_launcher
            path, digest = json.loads(descriptor)
            use_shared_launcher(Path(path), digest)
    result = observed_runner(verbosity=2).run(unittest.TestSuite([case]))
    if source_identity(script) != source:
        raise ValueError('self-test source changed during its worker')
    data = {'schema_version': 1, 'case': case.id(), 'source': source,
            'tests_run': result.testsRun, 'successful': result.wasSuccessful(),
            'outcome': result._case_outcome,
            'skipped': [[test.id(), reason] for test, reason in result.skipped],
            'failures': [test.id() for test, _ in result.failures],
            'errors': [test.id() for test, _ in result.errors],
            'unexpected_successes': [test.id() for test in result.unexpectedSuccesses],
            'expected_failures': [test.id() for test, _ in result.expectedFailures]}
    with report.open('x', encoding='utf-8') as output:
        json.dump(data, output, indent=2)
    return int(not result.wasSuccessful())


def run_cases(script, cases, *, fixture_launcher=None):
    from _windows_job import WindowsJob
    from ci_performance import task_directory, require_active_task
    names = [case._testMethodName for case in cases]
    expected = {name: case.id() for name, case in zip(names, cases)}
    if not names or len(expected) != len(names):
        raise ValueError('self-test inventory must be nonempty and unique')
    source = source_identity(script)
    active = task_directory()
    if active is not None:
        require_active_task(active)
    evidence = Path(tempfile.mkdtemp(prefix='execution-self-tests-', dir=active))
    print('Native self-test evidence: ' + str(evidence), flush=True)
    rows = []
    environment = dict(os.environ)
    environment.pop(SHARED_LAUNCHER, None)
    if fixture_launcher is not None:
        digest = hashlib.sha256(fixture_launcher.read_bytes()).hexdigest()
        environment[SHARED_LAUNCHER] = json.dumps((str(fixture_launcher), digest))
    stopping, launch_lock = Event(), Lock()
    processes = []
    acknowledgements = {}
    with WindowsJob() as job:
        def execute(name):
            report, log = evidence / (name + '.json'), evidence / (name + '.log')
            command = [sys.executable, str(script.with_name('deadline.py')),
                       '--windows-job-child', job.name, '--', sys.executable, '-B', '-X', 'utf8',
                       str(script), '--self-test', '--self-test-case', name,
                       '--self-test-report', str(report), '--self-test-admitted']
            print('Starting native self-test: ' + name, flush=True)
            with log.open('xb') as output:
                with launch_lock:
                    if stopping.is_set():
                        return name, 125, report, log
                    process = subprocess.Popen(command, stdin=subprocess.PIPE,
                                               stdout=subprocess.PIPE, stderr=output, bufsize=0,
                                               env=environment)
                    processes.append(process)
                acknowledgement = process.stdout.readline()
                with launch_lock:
                    try:
                        if not stopping.is_set() and process.poll() is None:
                            admitted = json.loads(acknowledgement)
                            if admitted.get('case') != expected[name] or admitted.get('source') != source:
                                raise ValueError('self-test worker readiness has a foreign case/source')
                            acknowledgements[name] = admitted
                            process.stdin.write(b'\x01')
                            process.stdin.flush()
                    finally:
                        process.stdin.close()
                # Drain native/Python stdout after the readiness frame; leaving
                # the pipe unread could block a case with substantial output.
                while chunk := process.stdout.read(65536):
                    output.write(chunk)
                    output.flush()
                status = process.wait()
                process.stdout.close()
            return name, status, report, log
        pool = ThreadPoolExecutor(max_workers=WORKERS)
        futures = []
        try:
            rank = {name: index for index, name in enumerate(EARLY_CASES)}
            dispatch = sorted(names, key=lambda name: rank.get(name, len(rank)))
            futures = [pool.submit(execute, name) for name in dispatch]
            for future in as_completed(futures):
                name, status, report, log = future.result()
                output = log.read_text(encoding='utf-8', errors='replace')
                print(output, end='', flush=True)
                row = {'case': expected[name], 'raw_exit': status, 'log': str(log),
                       'log_sha256': hashlib.sha256(log.read_bytes()).hexdigest(), 'report': str(report),
                       'admission': acknowledgements.get(name)}
                try:
                    data = validate_report(json.loads(report.read_bytes()), expected[name], source, status)
                    if '(' + expected[name] + ')' not in output:
                        raise ValueError('self-test worker log lacks its selected case identity')
                    row.update(outcome=data['outcome'], skipped=data['skipped'],
                               report_sha256=hashlib.sha256(report.read_bytes()).hexdigest())
                except (OSError, ValueError, TypeError) as error:
                    row.update(outcome='worker-failed', error=str(error))
                rows.append(row)
        finally:
            for future in futures:
                future.cancel()
            try:
                with launch_lock:
                    # Sealing, registering Popen and issuing permits share one
                    # lock. No permission can be sent after this transition.
                    stopping.set()
                    try:
                        for process in processes:
                            if not process.stdin.closed:
                                process.stdin.close()
                    finally:
                        try:
                            job.terminate_and_wait()
                        finally:
                            # A late attachment may already hold a Job handle.
                            # EOF also rejects its case before setup can run.
                            job.close()
            finally:
                try:
                    # The registry includes processes still entering their Job;
                    # waiting only Job members would omit those native handles.
                    for process in processes:
                        process.wait()
                finally:
                    try:
                        pool.shutdown(wait=True, cancel_futures=True)
                    finally:
                        for process in processes:
                            process.stdout.close()
    unchanged = source_identity(script) == source
    complete = len(rows) == len(expected) and {row['case'] for row in rows} == set(expected.values())
    passed = sum(row['outcome'] == 'passed' for row in rows)
    failed = not unchanged or not complete or any(row['outcome'] == 'worker-failed' for row in rows)
    summary = {'source': source, 'source_unchanged': unchanged, 'inventory_complete': complete,
               'expected_cases': list(expected.values()), 'rows': rows, 'passed': passed,
               'skipped': sum(row['outcome'] == 'skipped' for row in rows), 'raw_exit': int(failed),
               'workers': WORKERS, 'deadline': 'the single enclosing caller deadline; no per-worker reset'}
    with (evidence / 'results.json').open('x', encoding='utf-8') as output:
        json.dump(summary, output, indent=2)
    print(f"Native self-tests: {passed} passed, {summary['skipped']} skipped, "
          f"{len(rows)}/{len(expected)} cases observed; exit {int(failed)}", flush=True)
    return int(failed)
