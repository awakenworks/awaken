#!/usr/bin/env python3
"""Report CI/build costs without treating performance telemetry as qualification."""
from __future__ import annotations

import argparse
import csv
import datetime
import hashlib
import io
import json
import math
import os
import platform
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
import uuid
import urllib.error
import urllib.request

# The policy subcommand reads repository source. Report/cache subcommands read
# caller-supplied build observations; those runtime artifacts are not hook inputs.
INPUT_GLOBS = (".github/workflows/quality.yml",)


def task_directory(environment=None):
    environment = os.environ if environment is None else environment
    raw = environment.get('AWAKEN_BUILD_RUN_DIRECTORY')
    if not raw:
        return None
    path = Path(raw)
    if not path.is_absolute():
        raise ValueError('build run directory must be absolute')
    if not (path / 'evidence.json').is_file():
        # The host launcher can be the outermost owner (Cargo external command).
        # Its invocation is not a repository task bundle. Start a child task
        # with an explicit cross-run link rather than reusing a foreign schema.
        invocation = environment.get('AWAKEN_BUILD_INVOCATION_ID', '')
        if re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,127}', invocation):
            candidate = path / 'invocations' / invocation / 'observation.json'
            if candidate.is_file():
                value = json.loads(candidate.read_text())
                if (value.get('schema_version') == 1 and value.get('kind') == 'cargo-build-observation'
                        and value.get('run_id') == environment.get('AWAKEN_BUILD_RUN_ID')
                        and value.get('invocation_id') == invocation):
                    return None
        raise ValueError('build context has no matching task or host invocation')
    record = json.loads((path / 'evidence.json').read_text(encoding='utf-8'))
    if (record.get('kind') != 'build-observations' or record.get('observation_only') is not True
            or record.get('run_id') != environment.get('AWAKEN_BUILD_RUN_ID')):
        raise ValueError('build run context does not match its record')
    return path


def require_active_task(directory):
    record = json.loads((directory / 'evidence.json').read_text())
    if record.get('status') != 'running':
        raise ValueError('cannot append to or finish a completed build run')
    return record


class TaskRun:
    """Associate existing execution owners; reuse the diagnostic bundle writer."""
    def __init__(self, label, root, environment=None, *, workload=None, source_identity=None):
        from formal_run_evidence import begin_artifacts, write_record
        self.environment = dict(os.environ if environment is None else environment)
        self.directory = task_directory(self.environment)
        self.owned = self.directory is None
        self.commands_id = str(uuid.uuid4())
        if self.owned:
            base = self.environment.get('AWAKEN_BUILD_OBSERVATIONS')
            if not base:
                base = (str(Path(self.environment['RUNNER_TEMP']) / 'awaken-build-observations')
                        if self.environment.get('RUNNER_TEMP') else
                        str(Path.home() / '.local/state/awaken-builds'))
            if not Path(base).is_absolute():
                raise ValueError('build observations root must be absolute')
            self.directory = begin_artifacts(root, Path(base), 'build-observations', observation_only=True,
                                             source_identity=source_identity)
            path = self.directory / 'evidence.json'
            record = json.loads(path.read_text())
            record.update(label=label, started_monotonic=time.monotonic(), repository=str(root.resolve()),
                          parent_run_id=self.environment.get('AWAKEN_BUILD_RUN_ID'),
                          parent_invocation_id=self.environment.get('AWAKEN_BUILD_INVOCATION_ID'),
                          workload_digest=None if workload is None else hashlib.sha256(json.dumps(workload).encode()).hexdigest(),
                          host={'name': platform.node(), 'logical_cpus': os.cpu_count()},
                          workflow={key: self.environment[key] for key in (
                              'GITHUB_REPOSITORY', 'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT',
                              'GITHUB_JOB', 'RUNNER_NAME') if key in self.environment},
                          configuration={name: hashlib.sha256((root / name).read_bytes()).hexdigest()
                              for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml',
                                           '.config/rust-tests.toml', '.github/workflows/quality.yml')
                              if (root / name).is_file()},
                          scenario=self.environment.get('AWAKEN_BUILD_SCENARIO', 'unspecified'))
            record['configuration']['environment'] = {key: self.environment[key] for key in (
                'CARGO_BUILD_JOBS', 'CARGO_INCREMENTAL', 'CARGO_BUILD_TARGET') if key in self.environment}
            record['configuration']['rustflags_digest'] = hashlib.sha256(self.environment.get('RUSTFLAGS', '').encode()).hexdigest()
            write_record(path, record)
            self.environment.update(AWAKEN_BUILD_RUN_ID=record['run_id'],
                                    AWAKEN_BUILD_RUN_DIRECTORY=str(self.directory))
        self.root = root
        require_active_task(self.directory)

    def record_commands(self, rows):
        from formal_run_evidence import write_record
        require_active_task(self.directory)
        # Existing raw execution/evidence owners keep their original data. This
        # index deliberately excludes arbitrary command-line secrets and output.
        observed = []
        for row in rows:
            arguments = row['command']
            observed.append({**{key: row[key] for key in ('exit_code', 'duration_seconds',
                              'result_exit_code', 'semantic_result') if key in row},
                             'executable': Path(arguments[0]).name,
                             'command_digest': hashlib.sha256(json.dumps(arguments).encode()).hexdigest()})
        write_record(self.directory / 'commands' / (self.commands_id + '.json'),
                     {'run_id': self.environment['AWAKEN_BUILD_RUN_ID'], 'rows': observed})

    def finish(self, status):
        if self.owned:
            finish_task(self.directory, self.root, status)


def finish_task(directory, root, status):
    from formal_run_evidence import finish_artifacts, write_record
    path = directory / 'evidence.json'
    record = require_active_task(directory)
    duration = time.monotonic() - record['started_monotonic']
    record.update(duration_seconds=duration if duration >= 0 else None)
    # Freeze externally written stage files before the existing bundle owner
    # seals content digests. Later target rotation cannot change this run.
    for reference in (directory / 'references').glob('*.json'):
        value = json.loads(reference.read_text())
        original = Path(value['path'])
        if not original.is_absolute():
            continue
        if directory in original.parents:
            value['path'] = str(original.relative_to(directory))
        elif original.is_file():
            relative = Path('registered') / reference.stem / original.name
            (directory / relative).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(original, directory / relative)
            value['path'] = str(relative)
        else:
            value['unavailable'] = True
        write_record(reference, value)
    write_record(path, record)
    finished = finish_artifacts(directory, root, status)
    duration = time.monotonic() - record['started_monotonic']
    finished['duration_seconds'] = duration if duration >= 0 else None
    write_record(path, finished)
    return finished


def register_evidence(path, kind, environment=None):
    from formal_run_evidence import write_record
    directory = task_directory(environment)
    if directory is None:
        return
    require_active_task(directory)
    write_record(directory / 'references' / (str(uuid.uuid4()) + '.json'),
                 {'kind': kind, 'path': str(path.resolve())})


def archive_task(directory, destination):
    """Artifact copy of one selected run; never merge runs by source revision."""
    if destination.resolve() == directory.resolve():
        return directory
    if directory.resolve() in destination.resolve().parents:
        raise ValueError('archive must be outside the canonical build run')
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix='.build-observations-', dir=destination.parent))
    try:
        # Only the observation tree is copied, never target/deps or build outputs.
        for path in directory.rglob('*'):
            if path.is_symlink():
                raise ValueError('build observation tree contains a symlink')
        shutil.copytree(directory, temporary, dirs_exist_ok=True)
        if destination.exists():
            # An existing mirror is replaced only by this same canonical run.
            prior = json.loads((destination / 'evidence.json').read_text())
            current = json.loads((directory / 'evidence.json').read_text())
            if prior['run_id'] != current['run_id']:
                raise ValueError('build observation archive identity mismatch')
            shutil.rmtree(destination)
        os.replace(temporary, destination)
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)
    return destination


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
    for name in ('dependency_fetch', 'trusted_host_portability', 'full_quality'):
        job = re.search(r'^  ' + name + r':\n(.*?)(?=^  [a-z_]+:|\Z)', workflow, re.MULTILINE | re.DOTALL)
        body = job[1] if job else ''
        begin = 'run: python3 scripts/ci/ci_performance.py begin --label ' + name + ' --github'
        finish = 'run: python3 scripts/ci/ci_performance.py finish --conclusion "${{ job.status }}"'
        if body.count(begin) != 1 or body.count(finish) != 1 or body.find(begin) > body.find(finish):
            errors.append(name + ': requires one ordered build observation lifecycle')
        terminal = re.search(r'^      - name: Finish build observations\n(.*?)(?=^      - |\Z)', body, re.MULTILINE | re.DOTALL)
        if not terminal or "if: always() && env.AWAKEN_BUILD_RUN_ID != ''" not in terminal[1]:
            errors.append(name + ': must retain failed/cancelled build observations')
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


def percentile(values, fraction):
    if not values:
        return None
    values = sorted(values)
    if any(not math.isfinite(value) or value < 0 for value in values):
        raise ValueError('invalid duration sample')
    position = fraction * (len(values) - 1)
    lower = int(position)
    return values[lower] + (values[min(lower + 1, len(values) - 1)] - values[lower]) * (position - lower)


def test_costs(text):
    matches = re.findall(r'^\s*PASS\s+\[\s*([\d.]+)s\]\s+(?:\([^)]*\)\s+)?(\S+)\s+(\S+)', text, re.MULTILINE)
    if not matches:
        return {'observed_passes': 0, 'p50_seconds': None, 'p95_seconds': None, 'slowest': []}
    values = sorted(float(value) for value, _, _ in matches)
    if any(not math.isfinite(value) or value < 0 for value in values):
        raise ValueError('invalid test duration')
    return {'observed_passes': len(matches), 'p50_seconds': percentile(values, .5), 'p95_seconds': percentile(values, .95),
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


def local_report(root, revision, run_id=None):
    result = {'gate_timings': [], 'rust': [], 'build_timelines': [], 'benchmarks': [],
              'cache': [], 'missing': [], 'commands': [], 'cargo_invocations': []}
    if run_id is not None and str(uuid.UUID(run_id)) != run_id:
        raise ValueError('invalid selected build run id')
    directory = task_directory() if not run_id else root / 'build-runs' / run_id
    if directory is None:
        result['missing'].append('no build run selected; historical same-SHA files are not attributed')
        return result
    record = json.loads((directory / 'evidence.json').read_text())
    if record.get('kind') != 'build-observations' or record.get('observation_only') is not True:
        raise ValueError('not a build observation run')
    if run_id is not None and record['run_id'] != run_id:
        raise ValueError('selected run identity mismatch')
    if record['source'].get('commit') not in (None, revision):
        raise ValueError('selected build run belongs to another revision')
    result['run'] = record
    if record['source'].get('available') is False:
        result['missing'].append('full source snapshot remains with the execution owner; see its source digest/receipt')
    run_root = directory
    references = [json.loads(path.read_text()) for path in (run_root / 'references').glob('*.json')]
    for reference in references:
        path = Path(reference['path'])
        if not path.is_absolute():
            path = run_root / path
        if not path.exists():
            result['missing'].append(reference['kind'] + ': registered evidence unavailable')
        elif reference['kind'] == 'gate':
            result['gate_timings'].extend(timings(path))
        elif reference['kind'] == 'cache':
            result['cache'].append(json.loads(path.read_text()))
    for path in sorted((run_root / 'commands').glob('*.json')):
        value = json.loads(path.read_text())
        if value['run_id'] != record['run_id']:
            raise ValueError('foreign command observations')
        result['commands'].extend(value['rows'])
    for path in sorted((run_root / 'invocations').glob('*/observation.json')):
        value = json.loads(path.read_text())
        if value.get('schema_version') != 1 or value.get('kind') != 'cargo-build-observation' or value.get('run_id') != record['run_id']:
            raise ValueError('foreign or unsupported Cargo observation')
        result['cargo_invocations'].append(value)
    if not result['cargo_invocations']:
        result['missing'].append('host Cargo invocation capture unavailable or no Cargo build reached')
    for directory in sorted(run_root.glob('rust-gate.*')):
        if not (directory / 'source.txt').is_file() or (directory / 'source.txt').read_text(encoding='utf-8').strip() != revision:
            result['missing'].append('Rust evidence source mismatch')
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
            local_path = shard_path
            if local_path.is_file():
                shard = json.loads(local_path.read_text(encoding='utf-8'))
                row['partition'] = {'status': shard['status'], 'executed_count': shard.get('executed_count'),
                                    'seconds': shard['duration_seconds'], 'shards': len(shard['shards'])}
        result['rust'].append(row)
    for path in sorted(run_root.glob('build-benchmark-*/results.json')):
        data = json.loads(path.read_text(encoding='utf-8'))
        if data['source_head'] == revision:
            result['benchmarks'].append({'path': str(path), 'status': data['status'], 'source_unchanged': data.get('source_unchanged'),
                                        'packages': data['packages'], 'rows': [{key: row.get(key) for key in (
                                            'round', 'jobs', 'phase', 'elapsed_seconds', 'compilations', 'test_executable_bytes', 'exit_code')}
                                            for row in data['rows']]})
    for path in sorted((run_root / 'cargo-timings').rglob('cargo-timing-*.html')):
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
    local = data.get('local', {})
    if local.get('run'):
        run = local['run']
        lines.append(f"- Build run {run['run_id']}: {run['status']}; {run.get('duration_seconds')} s")
        calls = local.get('cargo_invocations', [])
        lines.append(f"- Observed Cargo calls: {len(calls)}; waiting and execution remain separate; nested durations are not summed.")
        for call in calls:
            lines.append(f"- Cargo {call['invocation_id']}: {call['status']}; queue {call.get('queue_seconds')} s; execution {call.get('execution_seconds')} s")
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
    parser.add_argument('mode', choices=('report', 'cache', 'github', 'workflow', 'begin', 'finish',
                                       'validate', 'register', 'stage', 'timings-before', 'timings-after', 'compare'), nargs='?', default='report')
    parser.add_argument('--root', type=Path, default=Path('target/test-timings'))
    parser.add_argument('--revision', default=os.environ.get('GITHUB_SHA'))
    parser.add_argument('--output', type=Path, default=Path('target/test-timings/performance.json'))
    parser.add_argument('--hit', choices=('true', 'false', 'unknown'), default='unknown')
    parser.add_argument('--run-id')
    parser.add_argument('--label', default='build')
    parser.add_argument('--shell-file', type=Path)
    parser.add_argument('--github', action='store_true')
    parser.add_argument('--exit-code', type=int, default=0)
    parser.add_argument('--elapsed', type=float)
    parser.add_argument('--kind', choices=('gate', 'cache'))
    parser.add_argument('--path', type=Path)
    parser.add_argument('--snapshot', type=Path)
    parser.add_argument('--conclusion', choices=('success', 'failure', 'cancelled'))
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
    if args.mode in ('begin', 'finish', 'validate', 'register', 'stage', 'timings-before', 'timings-after'):
        from formal_run_evidence import write_record
        repository = Path(__file__).resolve().parents[2]
        if args.mode == 'begin':
            task = TaskRun(args.label, repository)
            values = {key: task.environment[key] for key in ('AWAKEN_BUILD_RUN_ID', 'AWAKEN_BUILD_RUN_DIRECTORY')}
            if args.shell_file:
                args.shell_file.write_text(''.join('export ' + key + '=' + shlex.quote(value) + '\n'
                                                  for key, value in values.items()), encoding='utf-8')
            if args.github:
                with Path(os.environ['GITHUB_ENV']).open('a', encoding='utf-8') as stream:
                    for key, value in values.items():
                        if '\n' in value or '\r' in value:
                            raise ValueError('invalid workflow observation context')
                        stream.write(key + '=' + value + '\n')
            print('Build observations: ' + str(task.directory), file=sys.stderr)
            return 0
        directory = task_directory()
        if directory is None:
            if args.mode == 'validate' and os.environ.get('AWAKEN_BUILD_INVOCATION_ID'):
                return 3  # A validated host invocation needs a repository task context.
            raise ValueError('missing build run context')
        require_active_task(directory)
        if args.mode == 'finish':
            code = args.exit_code if args.conclusion is None else (0 if args.conclusion == 'success' else 130 if args.conclusion == 'cancelled' else 1)
            finish_task(directory, repository, code)
        elif args.mode == 'register':
            if args.path is None or args.kind is None:
                parser.error('register requires --path and --kind')
            register_evidence(args.path, args.kind)
        elif args.mode == 'stage':
            if args.elapsed is None or not math.isfinite(args.elapsed) or args.elapsed < 0:
                parser.error('stage requires a nonnegative finite --elapsed')
            write_record(directory / 'commands' / (str(uuid.uuid4()) + '.json'),
                         {'run_id': os.environ['AWAKEN_BUILD_RUN_ID'], 'rows': [
                             {'label': args.label, 'duration_seconds': args.elapsed, 'exit_code': args.exit_code}]})
        elif args.mode == 'timings-before':
            if args.path is None:
                parser.error('timings-before requires --path')
            snapshot = directory / 'timing-snapshots' / (str(uuid.uuid4()) + '.json')
            write_record(snapshot, {'path': str(args.path.resolve()), 'files': {
                path.name: [path.stat().st_mtime_ns, path.stat().st_size]
                for path in args.path.glob('cargo-timing-*.html')}})
            print(snapshot)
        elif args.mode == 'timings-after':
            if args.snapshot is None or directory.resolve() not in args.snapshot.resolve().parents:
                parser.error('timings-after requires this run\'s --snapshot')
            previous = json.loads(args.snapshot.read_text())
            for path in Path(previous['path']).glob('cargo-timing-*.html'):
                if previous['files'].get(path.name) != [path.stat().st_mtime_ns, path.stat().st_size]:
                    destination = directory / 'cargo-timings' / args.snapshot.stem / path.name
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(path, destination)
        return 0
    if args.mode == 'compare':
        groups = {}
        for path in (args.root / 'build-runs').glob('*/evidence.json'):
            run = json.loads(path.read_text())
            if run.get('observation_only') is not True or run.get('status') != 'passed' or run.get('source_unchanged') is not True:
                continue
            value = run.get('duration_seconds')
            if not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
                continue
            identity = {key: run.get(key) for key in ('label', 'workload_digest', 'configuration', 'platform', 'host', 'workflow', 'scenario')}
            # Workflow attempt identifies a sample, not a comparable workload.
            identity['workflow'] = {key: run.get('workflow', {}).get(key) for key in ('GITHUB_JOB', 'RUNNER_NAME')}
            key = json.dumps(identity, sort_keys=True)
            cohort = groups.setdefault(key, {'workload': identity, 'revisions': {}})
            # A stable dirty working tree is a valid diagnostic sample, but its
            # complete source identity must never collapse into its HEAD SHA.
            source_identity = json.dumps(run['source'], sort_keys=True)
            cohort['revisions'].setdefault(source_identity, []).append(value)
        cohorts = []
        for cohort in groups.values():
            rows = []
            for source_identity, values in cohort['revisions'].items():
                source = json.loads(source_identity)
                values.sort()
                rows.append({'revision': source.get('commit'), 'source': source,
                             'samples': len(values), 'p50_seconds': percentile(values, .5),
                             'p95_seconds': percentile(values, .95) if len(values) >= 20 else None})
            cohorts.append({'workload': cohort['workload'], 'observations': rows})
        write_report({'cohorts': cohorts, 'scope': 'diagnostic comparable cohorts; scenario is caller-declared, not inferred'}, args.output)
        return 0
    if args.mode == 'report' and args.path is not None:
        call = json.loads(args.path.read_text(encoding='utf-8'))
        if call.get('schema_version') != 1:
            raise ValueError('unsupported build observation schema')
        if call.get('kind') == 'cargo-build-observation':
            data = {'revision': call.get('source', {}).get('revision'),
                    'local': {'run': call, 'cargo_invocations': [call], 'missing': [],
                              'scope': 'one explicit invocation; shared cache and resource attribution are retained'}}
        elif call.get('kind') == 'build-observations' and call.get('observation_only') is True:
            revision = call['source'].get('commit')
            archive_task(args.path.parent.resolve(), args.root / 'build-runs' / call['run_id'])
            data = {'revision': revision, 'local': local_report(args.root, revision, call['run_id'])}
        else:
            raise ValueError('not a supported build observation record')
        write_report(data, args.output)
        return 0
    if not args.revision:
        selected_directory = task_directory() if not args.run_id else args.root / 'build-runs' / str(uuid.UUID(args.run_id))
        if selected_directory is not None:
            args.revision = json.loads((selected_directory / 'evidence.json').read_text()).get('source', {}).get('commit')
        if not args.revision:
            parser.error('--revision, GITHUB_SHA or a source-bound build run is required')
    if args.mode == 'cache':
        home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')))
        paths = [Path(os.environ.get('CARGO_TARGET_DIR', 'target')), home / 'registry', home / 'git']
        data = {'revision': args.revision, 'cache_hit': args.hit, 'paths': list(map(str, paths)),
                'scope': 'local Cargo directory footprint; excludes compressed archive and pre-existing runner cache attribution',
                'footprint': footprint(paths)}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(data, indent=2) + '\n', encoding='utf-8')
        register_evidence(args.output, 'cache')
    else:
        directory = task_directory()
        if directory is not None:
            record = json.loads((directory / 'evidence.json').read_text())
            archive_task(directory, args.root / 'build-runs' / record['run_id'])
        selected = args.run_id
        data = {'revision': args.revision}
        if args.mode == 'github':
            data['workflow'] = github_jobs(os.environ)
            candidates = []
            for path in (args.root / 'build-runs').glob('*/evidence.json'):
                record = json.loads(path.read_text())
                workflow = record.get('workflow', {})
                if (workflow.get('GITHUB_RUN_ID') == os.environ['GITHUB_RUN_ID']
                        and workflow.get('GITHUB_RUN_ATTEMPT') == os.environ['GITHUB_RUN_ATTEMPT']
                        and workflow.get('GITHUB_JOB') == 'full_quality'):
                    candidates.append(record['run_id'])
            if len(candidates) > 1:
                raise ValueError('multiple build runs claim this workflow job attempt')
            if candidates:
                selected = candidates[0]
        data['local'] = local_report(args.root, args.revision, selected)
        write_report(data, args.output)
    return 0


class PerformanceTests(unittest.TestCase):
    def fixture_task(self, label='fixture'):
        from formal_sources import detached_git_environment
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name) / 'repository'
        root.mkdir()
        environment = {key: value for key, value in detached_git_environment().items() if not key.startswith('AWAKEN_BUILD_')}
        environment.update(AWAKEN_BUILD_OBSERVATIONS=str(Path(temporary.name) / 'records'),
                           GIT_AUTHOR_NAME='Build tests', GIT_AUTHOR_EMAIL='build@example.test',
                           GIT_COMMITTER_NAME='Build tests', GIT_COMMITTER_EMAIL='build@example.test')
        subprocess.run(['git', 'init', '-q', str(root)], check=True, env=environment)
        (root / 'source').write_text('original\n')
        (root / '.gitignore').write_text('target/\narchive/\n')
        subprocess.run(['git', '-C', str(root), 'add', 'source', '.gitignore'], check=True, env=environment)
        subprocess.run(['git', '-C', str(root), '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture'], check=True, env=environment)
        return TaskRun(label, root, environment), root, environment

    def test_run_identity_reentry_and_same_revision_attempts_are_separate(self):
        # Cause graph I1 no context -> one new bundle; I2 inherited exact live
        # context -> same owner; I3 repeated same-SHA command -> distinct run.
        # I4 mismatched/finished context -> refuse, never append to another run.
        first, root, environment = self.fixture_task()
        nested = TaskRun('nested', root, first.environment)
        self.assertFalse(nested.owned)
        self.assertEqual(first.directory, nested.directory)
        second = TaskRun('fixture', root, environment)
        self.assertNotEqual(first.directory, second.directory)
        with self.assertRaises(ValueError):
            TaskRun('foreign', root, {**first.environment, 'AWAKEN_BUILD_RUN_ID': 'foreign'})
        first.finish(0)
        second.finish(23)
        with self.assertRaises(ValueError):
            TaskRun('finished', root, first.environment)
        # I20 terminal context remains readable for reports but no writer or
        # shell re-entry can append another attempt to its sealed record.
        with mock.patch.dict(os.environ, first.environment, clear=True), \
                mock.patch.object(sys, 'argv', [__file__, 'validate']):
            with self.assertRaises(ValueError):
                main()
        with self.assertRaises(ValueError):
            first.record_commands([])

    def test_host_external_command_context_becomes_a_linked_task(self):
        # I18 a valid outer Cargo invocation is a different record owner:
        # preserve its parent link and create one task, then reuse that task on
        # shell re-entry. A missing/mismatched host record cannot bless context.
        first, root, environment = self.fixture_task()
        host = root.parent / 'host'
        path = host / 'invocations/outer/observation.json'
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({'schema_version': 1, 'kind': 'cargo-build-observation', 'run_id': 'outer',
                                    'invocation_id': 'outer', 'status': 'running'}))
        incoming = {**environment, 'AWAKEN_BUILD_RUN_ID': 'outer',
                    'AWAKEN_BUILD_RUN_DIRECTORY': str(host), 'AWAKEN_BUILD_INVOCATION_ID': 'outer'}
        child = TaskRun('child', root, incoming)
        self.assertTrue(child.owned)
        record = json.loads((child.directory / 'evidence.json').read_text())
        self.assertEqual(record['parent_run_id'], 'outer')
        self.assertEqual(record['parent_invocation_id'], 'outer')
        self.assertFalse(TaskRun('reentry', root, child.environment).owned)
        with self.assertRaises(ValueError):
            TaskRun('mismatch', root, {**incoming, 'AWAKEN_BUILD_RUN_ID': 'foreign'})
        first.finish(0)
        child.finish(0)

    def test_real_capture_adapter_joins_observations_without_replacing_its_executor(self):
        # I19 an SDK/libtest-style capture adapter has a resolved environment:
        # execute it once, preserve its failure, and persist that same call in
        # a task bundle. An injected adapter is not automatically a test fake.
        from crate_test import execute_commands
        initial, root, environment = self.fixture_task()
        observed = []
        def capture(command, **kwargs):
            observed.append((command, kwargs['env']))
            return subprocess.CompletedProcess(command, 23)
        with mock.patch('formal_run_evidence.source_snapshot', side_effect=AssertionError('duplicate source scan')):
            status, rows = execute_commands([['capture', 'work']], environment, root=root, run=capture)
        self.assertEqual((status, len(observed), rows[0]['exit_code']), (23, 1, 23))
        directory = Path(observed[0][1]['AWAKEN_BUILD_RUN_DIRECTORY'])
        record = json.loads((directory / 'evidence.json').read_text())
        self.assertEqual((record['status'], record['exit_code']), ('failed', 23))
        self.assertEqual(len(list((directory / 'commands').glob('*.json'))), 1)
        initial.finish(0)

    def test_report_selects_one_attempt_and_never_adopts_unbound_same_sha_files(self):
        # I5 same SHA + two observed attempts -> only selected run's commands.
        # I6 an old unbound cargo HTML/TSV is present -> no accidental attribution.
        first, root, environment = self.fixture_task()
        second = TaskRun('fixture', root, environment)
        first.record_commands([{'command': ['cargo', 'check'], 'exit_code': 0, 'duration_seconds': 2}])
        second.record_commands([{'command': ['cargo', 'check'], 'exit_code': 0, 'duration_seconds': 99}])
        first.finish(0)
        second.finish(0)
        revision = json.loads((first.directory / 'evidence.json').read_text())['source']['commit']
        legacy = root / 'target/test-timings'
        legacy.mkdir(parents=True)
        (legacy / 'check-all.tsv').write_text('unbound data')
        with mock.patch.dict(os.environ, first.environment, clear=True):
            report = local_report(legacy, revision)
        self.assertEqual([row['duration_seconds'] for row in report['commands']], [2])
        self.assertEqual(report['gate_timings'], [])
        self.assertEqual(report['build_timelines'], [])

    def test_archive_is_relocatable_and_survives_loss_of_original_target_and_run(self):
        # I7 registered external TSV + completed run -> archive carries its own
        # exact data. Removing original target/run does not change the report.
        # I8 archive destination inside its source -> refuse recursive copying.
        task, root, environment = self.fixture_task()
        timing = root / 'target/timing.tsv'
        timing.parent.mkdir()
        timing.write_text('group\tcheck\tstatus\tduration_seconds\texit_code\tfailure_kind\n'
                          'rust\tcompile\tpassed\t3\t0\tnone\n')
        register_evidence(timing, 'gate', task.environment)
        task.finish(0)
        with self.assertRaises(ValueError):
            archive_task(task.directory, task.directory / 'recursive')
        destination = root / 'archive/build-runs' / task.environment['AWAKEN_BUILD_RUN_ID']
        archive_task(task.directory, destination)
        from formal_run_evidence import artifact_files
        self.assertEqual(artifact_files(destination), json.loads((destination / 'evidence.json').read_text())['files'])
        timing.unlink()
        shutil.rmtree(task.directory)
        with mock.patch.dict(os.environ, environment, clear=True):
            report = local_report(root / 'archive', json.loads((destination / 'evidence.json').read_text())['source']['commit'],
                                  task.environment['AWAKEN_BUILD_RUN_ID'])
        self.assertEqual(report['gate_timings'][0]['seconds'], 3)

    def test_source_changes_do_not_relabel_the_build_or_create_qualification(self):
        # I9 actual child succeeded + source changed -> raw success retained,
        # source_unchanged=false; telemetry can never pass artifact verification.
        from formal_run_evidence import verify_artifacts
        task, root, _ = self.fixture_task()
        (root / 'source').write_text('changed\n')
        task.finish(0)
        record = json.loads((task.directory / 'evidence.json').read_text())
        self.assertEqual(record['status'], 'passed')
        self.assertEqual(record['exit_code'], 0)
        self.assertFalse(record['source_unchanged'])
        self.assertFalse(record['full_gate'])
        with self.assertRaisesRegex(ValueError, 'cannot qualify'):
            verify_artifacts(task.directory, root)

    def test_missing_partial_and_foreign_invocations_are_explicit(self):
        # I10 unsupported host capture -> missing, not zero-cost success.
        # I11 interrupted/uncompleted child -> kept as running, not inferred PASS.
        # I12 foreign run id -> report refuses instead of mixing statistics.
        task, root, _ = self.fixture_task()
        revision = json.loads((task.directory / 'evidence.json').read_text())['source']['commit']
        with mock.patch.dict(os.environ, task.environment, clear=True):
            self.assertTrue(local_report(root, revision)['missing'])
            path = task.directory / 'invocations/call/observation.json'
            path.parent.mkdir(parents=True)
            value = {'schema_version': 1, 'kind': 'cargo-build-observation',
                     'run_id': task.environment['AWAKEN_BUILD_RUN_ID'], 'status': 'running'}
            path.write_text(json.dumps(value))
            self.assertEqual(local_report(root, revision)['cargo_invocations'][0]['status'], 'running')
            value['run_id'] = 'foreign'
            path.write_text(json.dumps(value))
            with self.assertRaises(ValueError):
                local_report(root, revision)

    def test_cargo_timing_snapshot_retains_only_reports_changed_by_this_stage(self):
        # I13 pre-existing timing file + one new report -> archive only the new
        # report; a complete absence remains missing rather than historical data.
        task, root, _ = self.fixture_task()
        timing = root / 'target/cargo-timings'
        timing.mkdir(parents=True)
        (timing / 'cargo-timing-old.html').write_text('old')
        snapshot = io.StringIO()
        import contextlib
        with mock.patch.dict(os.environ, task.environment, clear=True), \
                mock.patch.object(sys, 'argv', [__file__, 'timings-before', '--path', str(timing)]), \
                contextlib.redirect_stdout(snapshot):
            self.assertEqual(main(), 0)
        (timing / 'cargo-timing-new.html').write_text('new')
        with mock.patch.dict(os.environ, task.environment, clear=True), \
                mock.patch.object(sys, 'argv', [__file__, 'timings-after', '--snapshot', snapshot.getvalue().strip()]):
            self.assertEqual(main(), 0)
        self.assertEqual([path.name for path in (task.directory / 'cargo-timings').rglob('*.html')], ['cargo-timing-new.html'])

    def test_comparison_keeps_working_bytes_and_workload_separate(self):
        # I15 same HEAD + different stable working bytes -> separate source
        # samples. I16 different command workload -> separate cohort. I17 source
        # changes during execution -> excluded; small samples never invent p95.
        first, root, environment = self.fixture_task()
        archive = root / 'archive'
        tasks = [first]
        first.finish(0)
        (root / 'source').write_text('second\n')
        second = TaskRun('fixture', root, environment)
        second.finish(0)
        tasks.append(second)
        third = TaskRun('fixture', root, environment, workload=[['cargo', 'test', '-p', 'other']])
        third.finish(0)
        tasks.append(third)
        changed = TaskRun('fixture', root, environment)
        (root / 'source').write_text('late\n')
        changed.finish(0)
        tasks.append(changed)
        for task in tasks:
            archive_task(task.directory, archive / 'build-runs' / task.environment['AWAKEN_BUILD_RUN_ID'])
        output = root.parent / 'comparison.json'
        with mock.patch.dict(os.environ, environment, clear=True), \
                mock.patch.object(sys, 'argv', [__file__, 'compare', '--root', str(archive), '--output', str(output)]):
            self.assertEqual(main(), 0)
        cohorts = json.loads(output.read_text())['cohorts']
        self.assertEqual(sorted(len(row['observations']) for row in cohorts), [1, 2])
        observations = [row for cohort in cohorts for row in cohort['observations']]
        self.assertEqual(sum(row['samples'] for row in observations), 3)
        self.assertTrue(all(row['p95_seconds'] is None for row in observations))

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
        # I14 every build job begins before work and always finalizes failures;
        # removing or conditionally skipping either boundary loses coverage.
        for bad in (workflow.replace('begin --label full_quality --github', 'workflow', 1),
                    workflow.replace('finish --conclusion "${{ job.status }}"', 'validate', 1),
                    workflow.replace("if: always() && env.AWAKEN_BUILD_RUN_ID != ''", 'if: success()', 1)):
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
