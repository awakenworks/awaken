#!/usr/bin/env python3
"""Build once, partition a complete Rust inventory and reconcile its execution.

This owns only the ordinary Rust suite. Required live-backend and platform
behavior remains owned by the corresponding check-all groups; doctests remain
in check-rust.sh. Every shard shares one source, build, configuration and budget.
"""
from __future__ import annotations

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import nextest_execution
import rust_test_plan

ROOT = Path(__file__).resolve().parents[2]


def aggregate(expected: Counter, rows: list[dict], count: int, context: dict) -> Counter:
    if sorted(row['shard'] for row in rows) != list(range(1, count + 1)):
        raise ValueError('missing or duplicate shard')
    planned = Counter()
    actual = Counter()
    for row in rows:
        if row['context'] != context or row['exit_code'] != 0:
            raise ValueError('failed shard or different source/build/configuration')
        selected = Counter(map(tuple, row['selected_cases']))
        executed = Counter(map(tuple, row['executed_cases']))
        if not selected or selected != executed:
            raise ValueError('empty shard or different execution identities')
        planned.update(selected)
        actual.update(executed)
    if planned != expected or actual != expected or any(n != 1 for n in actual.values()):
        raise ValueError('shards do not cover the complete inventory exactly once')
    return actual


def metadata_arguments(arguments: list[str]) -> list[str]:
    result = ['cargo', 'metadata', '--format-version', '1']
    iterator = iter(arguments)
    for argument in iterator:
        key, equals, value = argument.partition('=')
        if key in ('--locked', '--offline', '--all-features', '--no-default-features'):
            result.append(argument)
        elif key in ('--features', '-F'):
            result.extend(['--features', value if equals else next(iterator)])
    return result


def execute(arguments: list[str], count: int, threads: int, budget: int) -> int:
    environment = dict(os.environ)
    directory = Path(environment.get('CARGO_TARGET_DIR', str(ROOT / 'target'))) / 'test-timings' / f'rust-shards-{time.time_ns()}'
    directory.mkdir(parents=True)
    digest, patch = rust_test_plan.source_snapshot()
    context = {'revision': rust_test_plan.git_output('rev-parse', 'HEAD').decode().strip(),
               'source_digest': digest, 'shard_count': count,
               'nextest_config_sha256': hashlib.sha256((ROOT / '.config/nextest.toml').read_bytes()).hexdigest(),
               'compiler_environment': {name: environment.get(name) for name in (
                   'CARGO_BUILD_JOBS', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')}}
    receipt = {'kind': 'complete-rust-test-partition', 'full_gate': False, 'status': 'running',
               'context': context, 'total_test_threads': threads, 'budget_seconds': budget,
               'rows': [], 'shards': [], 'fixture_scope': 'live backend/platform qualification belongs to its required gate'}
    (directory / 'source.patch').write_bytes(patch)
    receipt_path = directory / 'results.json'
    def save():
        receipt_path.write_text(json.dumps(receipt, indent=2) + '\n', encoding='utf-8')
    save()
    print('Rust partition evidence: ' + str(receipt_path), flush=True)
    started = time.monotonic()
    def stage(name, argv):
        remaining = budget - (time.monotonic() - started)
        row = {'stage': name, 'command': argv, 'log': str(directory / (name + '.log')),
               'stderr': str(directory / (name + '.stderr.log'))}
        began = time.monotonic()
        if remaining <= 0:
            row.update(exit_code=124, duration_seconds=0)
        else:
            with Path(row['log']).open('w', encoding='utf-8') as output, Path(row['stderr']).open('w', encoding='utf-8') as errors:
                row['exit_code'] = subprocess.run([sys.executable, str(ROOT / 'scripts/ci/deadline.py'),
                    str(remaining), '--', *argv], cwd=ROOT, env=environment, stdout=output, stderr=errors).returncode
            row['duration_seconds'] = time.monotonic() - began
        return row
    def required(name, argv):
        row = stage(name, argv)
        receipt['rows'].append(row)
        save()
        if row['exit_code']:
            raise RuntimeError(f"{name} exited {row['exit_code']}: {row['stderr']}")
        return Path(row['log'])
    status = 1
    try:
        binaries = required('binaries', ['cargo', 'nextest', 'list', *arguments, '--list-type', 'binaries-only', '--message-format', 'json'])
        metadata = required('metadata', metadata_arguments(arguments))
        context['binaries_sha256'] = hashlib.sha256(binaries.read_bytes()).hexdigest()
        context['metadata_sha256'] = hashlib.sha256(metadata.read_bytes()).hexdigest()
        reuse = ['--binaries-metadata', str(binaries), '--cargo-metadata', str(metadata)]
        inventory_path = required('inventory', ['cargo', 'nextest', 'list', *reuse, '--message-format', 'json'])
        inventory = json.loads(inventory_path.read_text(encoding='utf-8'))
        expected, excluded = nextest_execution.selection(inventory)
        receipt.update(selected_cases=sorted(expected.elements()), excluded_cases=excluded)
        plans = []
        for index in range(1, count + 1):
            partition = f'hash:{index}/{count}'
            listing = required(f'list-{index}', ['cargo', 'nextest', 'list', *reuse, '--partition', partition, '--message-format', 'json'])
            document = json.loads(listing.read_text(encoding='utf-8'))
            cases, _ = nextest_execution.selection(document)
            plans.append({'shard': index, 'partition': partition, 'inventory': document, 'selected_cases': sorted(cases.elements())})
        # Validate the partition union before launching any test process.
        aggregate(expected, [{**plan, 'context': context, 'exit_code': 0,
                  'executed_cases': plan['selected_cases']} for plan in plans], count, context)
        if rust_test_plan.source_snapshot()[0] != digest:
            raise ValueError('source changed before partition execution')
        per_shard = [threads // count + (index < threads % count) for index in range(count)]
        def run_shard(plan):
            index = plan['shard']
            argv = ['cargo', 'nextest', 'run', *reuse, '--partition', plan['partition'],
                    '--test-threads', str(per_shard[index - 1]), '--status-level', 'pass', '--final-status-level', 'fail',
                    '--success-output', 'never', '--failure-output', 'immediate-final', '--no-fail-fast',
                    '--color', 'never', '--hide-progress-bar', '--retries', '0']
            row = {**stage(f'test-{index}', argv), 'shard': index, 'context': context,
                   'selected_cases': plan['selected_cases'], 'test_threads': per_shard[index - 1]}
            row['command_exit_code'] = row['exit_code']
            try:
                output = Path(row['log']).read_text(encoding='utf-8') + '\n' + Path(row['stderr']).read_text(encoding='utf-8')
                row.update(nextest_execution.reconcile(plan['inventory'], output, row['exit_code']))
            except (OSError, ValueError, KeyError, TypeError) as error:
                row.update(exit_code=row['exit_code'] or 1, error=str(error))
            return row
        with ThreadPoolExecutor(max_workers=count) as pool:
            receipt['shards'] = list(pool.map(run_shard, plans))
        save()
        aggregate(expected, receipt['shards'], count, context)
        receipt['executed_count'] = sum(expected.values())
        receipt['configured_exclusions'] = len(excluded)
        status = 0
    except (OSError, ValueError, KeyError, TypeError, RuntimeError) as error:
        receipt['error'] = str(error)
        failed = [row['exit_code'] for row in [*receipt['rows'], *receipt['shards']] if row['exit_code']]
        status = failed[0] if failed else 1
        print(str(error), file=sys.stderr)
    finally:
        receipt['source_unchanged'] = rust_test_plan.source_snapshot()[0] == digest
        if not receipt['source_unchanged']:
            status = status or 1
            receipt['error'] = 'source changed during partition execution'
        receipt.update(status='passed' if status == 0 else 'failed', exit_code=status,
                       duration_seconds=time.monotonic() - started)
        save()
    return status


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument('--shards', type=int, default=1)
    parser.add_argument('--threads', type=int, default=os.cpu_count() or 1)
    parser.add_argument('--timeout', type=int, default=1800)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--worker', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(ShardTests)).wasSuccessful())
    arguments = args.arguments[1:] if args.arguments[:1] == ['--'] else args.arguments
    if not arguments or not 1 <= args.shards <= args.threads or args.timeout < 1:
        parser.error('nonempty build arguments, 1 <= shards <= threads and a positive timeout are required')
    allowed = ('--workspace', '-p', '--package', '--features', '-F', '--all-features', '--no-default-features', '--locked', '--offline')
    iterator = iter(arguments)
    for argument in iterator:
        key, equals, value = argument.partition('=')
        if key not in allowed:
            parser.error('unsupported build argument: ' + argument)
        if key in ('-p', '--package', '--features', '-F') and not equals:
            if next(iterator, None) is None:
                parser.error('missing value for ' + argument)
    if not args.worker:
        return subprocess.run([sys.executable, str(ROOT / 'scripts/ci/deadline.py'), str(args.timeout), '--',
                               sys.executable, str(Path(__file__).resolve()), '--worker', *sys.argv[1:]], cwd=ROOT).returncode
    return execute(arguments, args.shards, args.threads, args.timeout)


class ShardTests(unittest.TestCase):
    def test_failed_preparation_stops_before_test_launch_and_preserves_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls = []
            def run(argv, **kwargs):
                calls.append(argv)
                status = 23 if len(calls) == 2 else 0
                kwargs['stdout'].write('{}')
                return subprocess.CompletedProcess(argv, status)
            with mock.patch.dict(os.environ, {'CARGO_TARGET_DIR': temporary}), \
                 mock.patch.object(subprocess, 'run', side_effect=run), \
                 mock.patch.object(rust_test_plan, 'source_snapshot', return_value=('digest', b'')), \
                 mock.patch.object(rust_test_plan, 'git_output', return_value=b'candidate'), \
                 mock.patch.object(time, 'time_ns', return_value=123):
                self.assertEqual(execute(['--workspace', '--locked'], 2, 2, 30), 23)
            receipt = json.loads((Path(temporary) / 'test-timings/rust-shards-123/results.json').read_text())
            self.assertEqual(receipt['status'], 'failed')
            self.assertEqual([row['exit_code'] for row in receipt['rows']], [0, 23])
            self.assertEqual(receipt['shards'], [])
            self.assertEqual(len(calls), 2)

    def test_overlapping_inventories_fail_before_test_processes(self):
        inventory = {'test-count': 2, 'rust-suites': {'binary': {'binary-id': 'binary', 'status': 'listed',
            'testcases': {name: {'ignored': False, 'filter-match': {'status': 'matches'}} for name in ('a', 'b')}}}}
        with tempfile.TemporaryDirectory() as temporary:
            calls = []
            def run(argv, **kwargs):
                calls.append(argv)
                kwargs['stdout'].write(json.dumps(inventory))
                return subprocess.CompletedProcess(argv, 0)
            with mock.patch.dict(os.environ, {'CARGO_TARGET_DIR': temporary}), \
                 mock.patch.object(subprocess, 'run', side_effect=run), \
                 mock.patch.object(rust_test_plan, 'source_snapshot', return_value=('digest', b'')), \
                 mock.patch.object(rust_test_plan, 'git_output', return_value=b'candidate'), \
                 mock.patch.object(time, 'time_ns', return_value=123):
                self.assertEqual(execute(['--workspace', '--locked'], 2, 2, 30), 1)
            receipt = json.loads((Path(temporary) / 'test-timings/rust-shards-123/results.json').read_text())
            self.assertEqual(receipt['status'], 'failed')
            self.assertEqual(receipt['shards'], [])
            self.assertEqual(len(calls), 5)

    def test_union_rejects_missing_duplicate_renamed_failed_and_foreign_shards(self):
        context = {'revision': 'candidate', 'config': 'same'}
        expected = Counter({('binary-a', 'case'): 1, ('binary-b', 'case'): 1})
        def rows():
            return [{'shard': n, 'context': dict(context), 'exit_code': 0,
                     'selected_cases': [[name, 'case']], 'executed_cases': [[name, 'case']]}
                    for n, name in enumerate(('binary-a', 'binary-b'), 1)]
        self.assertEqual(aggregate(expected, rows(), 2, context), expected)
        for scenario in ('missing', 'duplicate-id', 'duplicate-test', 'renamed', 'failed', 'source', 'config'):
            sample = rows()
            if scenario == 'missing': sample.pop()
            elif scenario == 'duplicate-id': sample[1]['shard'] = 1
            elif scenario == 'duplicate-test': sample[1].update(selected_cases=sample[0]['selected_cases'], executed_cases=sample[0]['executed_cases'])
            elif scenario == 'renamed': sample[1]['executed_cases'] = [['binary-b', 'renamed']]
            elif scenario == 'failed': sample[1]['exit_code'] = 124
            else: sample[1]['context'][{'source': 'revision', 'config': 'config'}[scenario]] = 'different'
            with self.subTest(scenario=scenario), self.assertRaises(ValueError):
                aggregate(expected, sample, 2, context)

    def test_metadata_preserves_feature_and_lock_selection(self):
        self.assertEqual(metadata_arguments(['--workspace', '--locked', '-p', 'crate', '--features', 'crate/pg', '--no-default-features']),
                         ['cargo', 'metadata', '--format-version', '1', '--locked', '--features', 'crate/pg', '--no-default-features'])


if __name__ == '__main__':
    raise SystemExit(main())
