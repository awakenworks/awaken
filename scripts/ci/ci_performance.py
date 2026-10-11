#!/usr/bin/env python3
"""Report CI/build costs without treating performance telemetry as qualification."""
from __future__ import annotations

import argparse
import csv
import datetime
import json
import math
import os
from pathlib import Path
import re
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request

# The policy subcommand reads repository source. Report/cache subcommands read
# caller-supplied build observations; those runtime artifacts are not hook inputs.
INPUT_GLOBS = (".github/workflows/quality.yml",)


def artifact_policy_errors(workflow):
    """Keep compiler payload out of observations without dropping gate evidence."""
    exclusion = '!target/test-timings/build-benchmark-*/round-*-jobs-*/debug/**'
    common = {'target/test-timings/', 'target/cargo-timings/', exclusion}
    owners = {
        'Retain native build and cache observations': common,
        'Retain exact-SHA quality evidence': common | {'web/test-results/', 'scripts/ci/gate_record.json'},
    }
    errors = []
    for owner, expected in owners.items():
        blocks = re.findall(r'^      - name: ' + re.escape(owner) +
                            r'\n(.*?)(?=^      - |^  [A-Za-z_]+:|\Z)', workflow, re.MULTILINE | re.DOTALL)
        if len(blocks) != 1:
            errors.append(owner + ': missing or duplicate observation upload')
            continue
        paths = re.findall(r'^          path: \|\n((?:^            [^\n]+\n)+)', blocks[0], re.MULTILINE)
        actual = paths[0].splitlines() if len(paths) == 1 else []
        actual = [path.strip() for path in actual]
        if set(actual) != expected or len(actual) != len(expected):
            errors.append(owner + ': retain raw evidence and exclude only benchmark debug payload')
    return errors


def seconds(start, end):
    if start is None or end is None:
        return None
    value = (datetime.datetime.fromisoformat(end.replace('Z', '+00:00')) -
             datetime.datetime.fromisoformat(start.replace('Z', '+00:00'))).total_seconds()
    if not math.isfinite(value) or value < 0:
        raise ValueError('invalid step duration')
    return value


def jobs_report(jobs, run_id, attempt, workflow_head):
    if not jobs or len({job['id'] for job in jobs}) != len(jobs):
        raise ValueError('empty or duplicate workflow jobs')
    rows = []
    for job in jobs:
        if job['run_id'] != run_id or job['run_attempt'] != attempt or job['head_sha'] != workflow_head:
            raise ValueError('workflow jobs are from different source/run/attempt')
        if job['name'] == 'performance_summary':
            continue  # This observer is still running, after the measured jobs.
        steps = [{'name': step['name'], 'conclusion': step['conclusion'],
                  'seconds': seconds(step.get('started_at'), step.get('completed_at'))}
                 for step in job['steps']]
        rows.append({'job': job['name'], 'status': job['status'], 'conclusion': job['conclusion'],
                     'runner': job.get('runner_name'), 'seconds': seconds(job.get('started_at'), job.get('completed_at')),
                     'steps': steps, 'cache_steps': [step for step in steps if 'Rust dependency cache' in step['name']]})
    if not rows:
        raise ValueError('no measured workflow jobs')
    return rows


def timings(path):
    with path.open(encoding='utf-8-sig', newline='') as stream:
        rows = list(csv.DictReader(stream, delimiter='\t'))
    if not rows or len({(row.get('group'), row.get('check', row.get('stage'))) for row in rows}) != len(rows):
        raise ValueError('empty or duplicate stage timing rows')
    for row in rows:
        value = float(row.get('duration_seconds', row.get('elapsed_seconds')))
        status = int(row['exit_code'])
        if value < 0 or not math.isfinite(value) or row.get('status', 'passed' if status == 0 else 'failed') != ('passed' if status == 0 else 'failed'):
            raise ValueError('invalid timing status or duration')
        row.update(seconds=value, exit_code=status)
    return sorted(rows, key=lambda row: row['seconds'], reverse=True)


def test_costs(text):
    matches = re.findall(r'^\s*PASS\s+\[\s*([\d.]+)s\]\s+(?:\([^)]*\)\s+)?(\S+)\s+(\S+)', text, re.MULTILINE)
    if not matches:
        return {'observed_passes': 0, 'p50_seconds': None, 'p95_seconds': None, 'slowest': []}
    values = sorted(float(value) for value, _, _ in matches)
    if any(not math.isfinite(value) or value < 0 for value in values):
        raise ValueError('invalid test duration')
    def percentile(fraction):
        position = fraction * (len(values) - 1)
        lower = int(position)
        return values[lower] + (values[min(lower + 1, len(values) - 1)] - values[lower]) * (position - lower)
    return {'observed_passes': len(matches), 'p50_seconds': percentile(.5), 'p95_seconds': percentile(.95),
            'slowest': [{'binary': binary, 'test': name, 'seconds': float(value)}
                        for value, binary, name in sorted(matches, key=lambda row: float(row[0]), reverse=True)[:15]]}


def footprint(paths, budget=5):
    started = time.monotonic()
    total = files = 0
    for path in paths:
        for parent, directories, names in os.walk(path, followlinks=False):
            directories[:] = [name for name in directories if not (Path(parent) / name).is_symlink()]
            for name in names:
                if time.monotonic() - started > budget:
                    return {'bytes': None, 'files': None, 'complete': False, 'scan_seconds': time.monotonic() - started}
                item = Path(parent) / name
                try:
                    if not item.is_symlink():
                        total += item.stat().st_size
                        files += 1
                except OSError:
                    return {'bytes': None, 'files': None, 'complete': False, 'scan_seconds': time.monotonic() - started}
    return {'bytes': total, 'files': files, 'complete': True, 'scan_seconds': time.monotonic() - started}


def local_report(root, revision):
    result = {'gate_timings': [], 'rust': [], 'build_timelines': [], 'benchmarks': [], 'cache': [], 'missing': []}
    gate_path = root / 'check-all.tsv'
    if gate_path.is_file():
        result['gate_timings'] = timings(gate_path)
    else:
        result['missing'].append('check-all.tsv: complete CI domain costs unavailable')
    for directory in sorted(root.glob('rust-gate.*')):
        if not (directory / 'source.txt').is_file() or (directory / 'source.txt').read_text(encoding='utf-8').strip() != revision:
            continue
        row = {'path': str(directory), 'revision': revision,
               'working_tree_clean': not (directory / 'working-tree.patch').read_bytes(),
               'stages': timings(directory / 'timings.tsv')}
        if (directory / 'test.log').is_file():
            row['test_costs'] = test_costs((directory / 'test.log').read_text(encoding='utf-8'))
        shard_line = re.search(r'Rust partition evidence: (.+)', (directory / 'test.log').read_text(encoding='utf-8') if (directory / 'test.log').is_file() else '')
        if shard_line:
            shard_path = Path(shard_line[1].strip())
            # Original absolute paths do not survive artifact download. Resolve
            # by owned basename within the downloaded timing tree instead.
            local_path = root / shard_path.parent.name / 'results.json'
            if local_path.is_file():
                shard = json.loads(local_path.read_text(encoding='utf-8'))
                row['partition'] = {'status': shard['status'], 'executed_count': shard.get('executed_count'),
                                    'seconds': shard['duration_seconds'], 'shards': len(shard['shards'])}
        result['rust'].append(row)
    for path in sorted(root.glob('build-benchmark-*/results.json')):
        data = json.loads(path.read_text(encoding='utf-8'))
        if data['source_head'] == revision:
            result['benchmarks'].append({'path': str(path), 'status': data['status'], 'source_unchanged': data.get('source_unchanged'),
                                        'packages': data['packages'], 'rows': [{key: row.get(key) for key in (
                                            'round', 'jobs', 'phase', 'elapsed_seconds', 'compilations', 'test_executable_bytes', 'exit_code')}
                                            for row in data['rows']]})
    for path in sorted(root.glob('cache-*.json')):
        data = json.loads(path.read_text(encoding='utf-8'))
        if data['revision'] == revision:
            result['cache'].append(data)
    cargo_root = root.parent / 'cargo-timings'
    for path in sorted(cargo_root.glob('cargo-timing-*.html')):
        text = path.read_text(encoding='utf-8')
        marker = 'const UNIT_DATA ='
        if marker not in text:
            continue
        units, _ = json.JSONDecoder().raw_decode(text[text.index(marker) + len(marker):].lstrip())
        result['build_timelines'].append({'path': str(path), 'scope': 'diagnostic units; source association requires the matching build receipt',
            'sum_unit_seconds': sum(unit['duration'] for unit in units),
            'longest_units': [{key: unit.get(key) for key in ('name', 'target', 'mode', 'duration')}
                              for unit in sorted(units, key=lambda unit: unit['duration'], reverse=True)[:10]]})
    return result


def github_jobs(environment):
    repository = environment['GITHUB_REPOSITORY']
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('invalid GitHub repository')
    run_id, attempt = int(environment['GITHUB_RUN_ID']), int(environment['GITHUB_RUN_ATTEMPT'])
    token = environment['GH_TOKEN']
    if run_id <= 0 or attempt <= 0 or not token:
        raise ValueError('missing workflow run/attempt or read token')
    def get(path):
        request = urllib.request.Request('https://api.github.com/repos/' + repository + path,
            headers={'Accept': 'application/vnd.github+json', 'Authorization': 'Bearer ' + token})
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    run = get(f'/actions/runs/{run_id}/attempts/{attempt}')
    jobs = []
    for page in range(1, 11):
        data = get(f'/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100&page={page}')
        jobs.extend(data['jobs'])
        if len(jobs) >= data['total_count']:
            break
    else:
        raise ValueError('incomplete workflow job pages')
    cache = get('/actions/caches?per_page=100')
    return {'repository': repository, 'run_id': run_id, 'run_attempt': attempt,
            'candidate_revision': environment['GITHUB_SHA'], 'workflow_head_sha': run['head_sha'],
            'jobs': jobs_report(jobs, run_id, attempt, run['head_sha']),
            'rust_cache_archives': [entry for entry in cache['actions_caches'] if entry['key'].startswith('v0-rust')],
            'cache_inventory_complete': cache['total_count'] <= 100,
            'cache_scope': 'listed archives; no claim that all listed entries were restored by this run'}


def write_report(data, output):
    output.parent.mkdir(parents=True, exist_ok=True)
    data.update(kind='ci-performance-telemetry', full_gate=False, status='observed')
    output.write_text(json.dumps(data, indent=2) + '\n', encoding='utf-8')
    lines = ['## CI performance observations', '', 'Performance telemetry is separate from gate qualification.', '']
    for job in data.get('workflow', {}).get('jobs', []):
        label = job['job'].replace('|', '\\|').replace('\n', ' ')
        lines.append(f"- {label}: {job['seconds']} s; {job['conclusion']}")
        for step in job['cache_steps']:
            lines.append(f"  - {step['name']}: {step['seconds']} s; {step['conclusion']}")
    for row in data.get('local', {}).get('gate_timings', [])[:10]:
        lines.append(f"- {row['group']}/{row['check']}: {row['seconds']} s; exit {row['exit_code']}")
    for row in data.get('local', {}).get('rust', []):
        costs = row.get('test_costs', {})
        lines.append(f"- Rust observed passes: {costs.get('observed_passes')}; p50 {costs.get('p50_seconds')} s; p95 {costs.get('p95_seconds')} s")
    for missing in data.get('local', {}).get('missing', []):
        lines.append('- Unavailable: ' + missing)
    text = '\n'.join(lines) + '\n'
    output.with_suffix('.md').write_text(text, encoding='utf-8')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a', encoding='utf-8') as stream:
            stream.write(text)
    print('Performance evidence: ' + str(output))


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument('mode', choices=('report', 'cache', 'github', 'workflow'), nargs='?', default='report')
    parser.add_argument('--root', type=Path, default=Path('target/test-timings'))
    parser.add_argument('--revision', default=os.environ.get('GITHUB_SHA'))
    parser.add_argument('--output', type=Path, default=Path('target/test-timings/performance.json'))
    parser.add_argument('--hit', choices=('true', 'false', 'unknown'), default='unknown')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(PerformanceTests)).wasSuccessful())
    if args.mode == 'workflow':
        errors = artifact_policy_errors(Path('.github/workflows/quality.yml').read_text(encoding='utf-8'))
        if errors:
            raise ValueError('; '.join(errors))
        print('Performance artifact policy: compiler payload excluded; raw gate evidence retained')
        return 0
    if not args.revision:
        parser.error('--revision or GITHUB_SHA is required')
    if args.mode == 'cache':
        home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')))
        paths = [Path(os.environ.get('CARGO_TARGET_DIR', 'target')), home / 'registry', home / 'git']
        data = {'revision': args.revision, 'cache_hit': args.hit, 'paths': list(map(str, paths)),
                'scope': 'local Cargo directory footprint; excludes compressed archive and pre-existing runner cache attribution',
                'footprint': footprint(paths)}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(data, indent=2) + '\n', encoding='utf-8')
    else:
        data = {'revision': args.revision, 'local': local_report(args.root, args.revision)}
        if args.mode == 'github':
            data['workflow'] = github_jobs(os.environ)
        write_report(data, args.output)
    return 0


class PerformanceTests(unittest.TestCase):
    def test_upload_owners_cannot_mask_missing_exclusions_or_lost_evidence(self):
        workflow = (Path(__file__).resolve().parents[2] / '.github/workflows/quality.yml').read_text(encoding='utf-8')
        self.assertEqual(artifact_policy_errors(workflow), [])
        exclusion = '!target/test-timings/build-benchmark-*/round-*-jobs-*/debug/**'
        # C1 both owners exclude compiler payload, C2 both retain logs and
        # canonical qualification files. Reject either missing exclusion,
        # broad evidence deletion, duplicate owner or unrelated lookalike.
        first = workflow.index(exclusion)
        for bad in (workflow[:first] + workflow[first:].replace(exclusion, 'removed', 1),
                    workflow[:first + len(exclusion)] + workflow[first + len(exclusion):].replace(exclusion, 'removed', 1),
                    workflow.replace(exclusion, '!target/test-timings/**'),
                    workflow.replace('            scripts/ci/gate_record.json\n', ''),
                    workflow.replace('Retain native build and cache observations', 'Unrelated cache upload'),
                    workflow + '\n      - name: Retain exact-SHA quality evidence\n'):
            with self.subTest(bad=bad[-80:]):
                self.assertTrue(artifact_policy_errors(bad))

    def test_exhausted_footprint_budget_reports_unavailable_size(self):
        with tempfile.TemporaryDirectory() as temporary:
            (Path(temporary) / 'artifact').write_bytes(b'actual contents')
            data = footprint([Path(temporary)], budget=0)
            self.assertFalse(data['complete'])
            self.assertIsNone(data['bytes'])
            self.assertIsNone(data['files'])

    def test_timing_rows_preserve_failure_and_reject_duplicates_nan_and_status_mismatch(self):
        header = 'group\tcheck\tstatus\tduration_seconds\texit_code\tfailure_kind\n'
        row = 'rust\tcompile\tfailed\t2.5\t75\tresource-unavailable\n'
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'timings.tsv'
            path.write_text(header + row)
            self.assertEqual(timings(path)[0]['exit_code'], 75)
            for bad in (header, header + row + row, header + row.replace('2.5', 'nan'), header + row.replace('failed', 'passed')):
                path.write_text(bad)
                with self.subTest(bad=bad), self.assertRaises(ValueError): timings(path)

    def test_source_attempt_duplicate_and_incomplete_job_controls(self):
        def job():
            return {'id': 1, 'run_id': 2, 'run_attempt': 3, 'head_sha': 'head', 'name': 'gate',
                    'status': 'completed', 'conclusion': 'failure', 'started_at': '2026-01-01T00:00:00Z',
                    'completed_at': '2026-01-01T00:00:05Z', 'steps': []}
        self.assertEqual(jobs_report([job()], 2, 3, 'head')[0]['conclusion'], 'failure')
        for field in ('run_id', 'run_attempt', 'head_sha'):
            sample = job()
            sample[field] = 'foreign'
            with self.subTest(field=field), self.assertRaises(ValueError):
                jobs_report([sample], 2, 3, 'head')
        for sample in ([], [job(), job()]):
            with self.assertRaises(ValueError): jobs_report(sample, 2, 3, 'head')
        sample = job()
        sample.update(completed_at=None, conclusion=None, status='in_progress')
        self.assertIsNone(jobs_report([sample], 2, 3, 'head')[0]['seconds'])

    def test_parallel_durations_are_not_added_into_wall_time(self):
        self.assertEqual(seconds('2026-01-01T00:00:00Z', '2026-01-01T00:00:05Z'), 5)
        with self.assertRaises(ValueError): seconds('2026-01-01T00:00:05Z', '2026-01-01T00:00:00Z')
        self.assertIsNone(seconds(None, None))

    def test_quantiles_include_non_progress_singular_output_and_missing_data(self):
        self.assertIsNone(test_costs('')['p95_seconds'])
        costs = test_costs('PASS [ 1.0s] binary-a case\nPASS [ 3.0s] (2/2) binary-b case')
        self.assertEqual(costs['observed_passes'], 2)
        self.assertEqual(costs['p50_seconds'], 2)
        self.assertAlmostEqual(costs['p95_seconds'], 2.9)


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, urllib.error.HTTPError) as error:
        print('Performance telemetry failed: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
