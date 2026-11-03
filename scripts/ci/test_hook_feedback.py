#!/usr/bin/env python3
"""Cause/effect contracts for native hook grouping and durable live feedback."""
import contextlib
import io
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import ci_performance
import generate_hooks
import parallel_check
from _build_observation_fixture import real_build_observations
from _strict_unittest import main

ROOT = Path(__file__).resolve().parents[2]
INPUT_GLOBS = ('lefthook.yml', 'scripts/ci/pre-commit/observe.sh',
               'scripts/ci/_build_observation.sh', 'scripts/ci/generate_hooks.py',
               'scripts/ci/parallel_check.py', 'scripts/ci/ci_performance.py',
               'scripts/ci/deadline.py', 'scripts/ci/_native_shell.py')


def transport(name, body, readonly=True):
    return generate_hooks.observed_hook_run(name, body, readonly=readonly, timeout=10)


def leaf(name, body, *, readonly=True, indent=4, glob=None):
    prefix = ' ' * indent
    value = prefix + '- name: ' + name + '\n'
    if readonly:
        value += prefix + '  tags: [read-only]\n'
    if glob:
        value += prefix + '  glob: "' + glob + '"\n'
    return value + prefix + '  run: ' + transport(name, body, readonly) + '\n'


def config(jobs):
    return ('output: [failure, execution_out, summary]\nglob_matcher: doublestar\n'
            'source_dir: scripts/ci\npre-commit:\n  follow: true\n  scripts:\n    observe.sh:\n'
            '      runner: python3 scripts/ci/_native_shell.py\n      args: --observe\n'
            'commit-checks:\n  parallel: false\n  piped: true\n  follow: true\n  jobs:\n' + jobs
            + '\ncommit-msg:\n  commands: {}\n')


class HookFeedbackTests(unittest.TestCase):
    def test_progress_reports_elapsed_stage_and_log_without_replaying_output(self):
        # H1 alive worker => elapsed/current Rust stage/log are visible; H2
        # stop => no further heartbeat; H3 private arbitrary child output is
        # retained in the log, without being replayed as progress. Controlled
        # event boundaries exercise the real heartbeat without a timing sleep.
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as raw:
            log = Path(raw) / 'output.log'
            log.write_text('private output\n[stage] check: started; log nested.log\n', encoding='utf-8')
            stopped = Mock()
            stopped.wait.side_effect = [False, True]
            stopped.is_set.return_value = False
            output = io.StringIO()
            with patch.object(parallel_check.time, 'monotonic', return_value=23), contextlib.redirect_stdout(output):
                parallel_check.hook_progress(stopped, 'rust-quick', log, 3)
            self.assertIn('[running] rust-quick: 20s; [stage] check:', output.getvalue())
            self.assertIn(str(log), output.getvalue())
            self.assertNotIn('private output', output.getvalue())
            self.assertEqual(output.getvalue().count('[running]'), 1)

    def fixture(self):
        task, root, environment = real_build_observations(self)
        return task, root, {key: value for key, value in task.environment.items()
                            if not key.startswith('LEFTHOOK')}

    def worker(self, task, root, env, body, *, readonly=True, args=(), timeout=10):
        command = [sys.executable, str(ROOT / 'scripts/ci/parallel_check.py'),
                   '--hook-label', 'fixture', '--timeout', str(timeout), '--shell', body]
        if readonly:
            command.append('--read-only')
        return subprocess.run([*command, '--', *args], cwd=root, env=env,
                              capture_output=True, text=True, timeout=20)

    def private_ci(self, root):
        scripts = root / 'scripts/ci'
        scripts.mkdir(parents=True)
        for module in {'parallel_check.py', '_native_shell.py', 'ci_performance.py',
                       *generate_hooks.local_imports('parallel_check.py'),
                       *generate_hooks.local_imports('_native_shell.py')}:
            shutil.copyfile(ROOT / 'scripts/ci' / module, scripts / module)
        shutil.copyfile(ROOT / 'scripts/ci/_build_observation.sh', scripts / '_build_observation.sh')
        (scripts / 'pre-commit').mkdir()
        shutil.copyfile(ROOT / 'scripts/ci/pre-commit/observe.sh', scripts / 'pre-commit/observe.sh')
        (scripts / 'pre-commit/observe.sh').chmod(0o755)
        (root / '.gitignore').write_text('target/\narchive/\n__pycache__/\n*.pyc\n')
        return scripts

    def test_parser_and_native_inventory_have_one_view(self):
        # I1 observed job + literal staged template -> same original checker
        # and argv, preserving its declared trigger. I2 nested and flat source
        # views have identical owners. I3 compare the production parser with
        # native Lefthook's JSON dump, not another hand-written job inventory.
        command = 'python3 scripts/ci/check_alpha.py --staged {staged_files}'
        raw = config('    - name: readers\n      group:\n        parallel: true\n        jobs:\n'
                     + leaf('alpha', command, indent=10))
        self.assertEqual(generate_hooks.hook_execution_errors(raw), [])
        blocks = generate_hooks.command_blocks(raw)
        self.assertEqual(generate_hooks.hook_run(blocks['alpha']), command)
        self.assertEqual(generate_hooks.invoked_scripts(blocks['alpha']), {'check_alpha.py'})
        declared = {'check_alpha.py': ('docs/*.md',)}
        rendered = generate_hooks.render(raw, declared)
        self.assertEqual(generate_hooks.render(rendered, declared), rendered)
        self.assertIn('docs/*.md', rendered)
        self.assertEqual(generate_hooks.wired_scripts(rendered), {'check_alpha.py'})
        self.assertEqual(generate_hooks.wired_scripts(generate_hooks.pre_commit_view(rendered)), {'check_alpha.py'})
        native_view = generate_hooks.native_pre_commit_configuration(rendered)
        self.assertNotIn('commit-checks:', native_view)
        self.assertEqual(generate_hooks.command_blocks(native_view), generate_hooks.command_blocks(rendered))
        for broken in (rendered.replace('args: --observe', 'args: --skip'),
                       rendered.replace('observe.sh:', 'elsewhere.sh:'),
                       rendered.replace('commit-checks:', 'unused-checks:')):
            with self.assertRaises(ValueError):
                generate_hooks.native_pre_commit_configuration(broken)
        # Universal declarations have multi-line generated comments; nesting
        # must retain both lines and the renderer's fixpoint in the flat view.
        universal = generate_hooks.render(raw, {'check_alpha.py': ('**/*',)})
        flat = generate_hooks.pre_commit_view(universal)
        self.assertEqual(generate_hooks.render(flat, {'check_alpha.py': ('**/*',)}), flat)
        # I4 a literal multi-command body retains its line boundaries, while
        # options after run (notably native staging) survive the logical view.
        from _input_closure import invocations
        literal = config('    - name: alpha\n      tags: [read-only]\n      run: |-\n'
                         '        python3 scripts/ci/check_alpha.py\n'
                         '        python3 scripts/ci/check_beta.py\n')
        generated = generate_hooks.render(literal, {**declared, 'check_beta.py': ('src/**',)})
        self.assertEqual(generate_hooks.hook_run(generate_hooks.command_blocks(generated)['alpha']),
                         'python3 scripts/ci/check_alpha.py\npython3 scripts/ci/check_beta.py')
        self.assertEqual(invocations(generated), [('alpha', ['scripts/ci/check_alpha.py']), ('alpha', ['scripts/ci/check_beta.py'])])
        writer = config(leaf('writer', 'rustfmt {staged_files}', readonly=False) + '      stage_fixed: true\n')
        self.assertIn('stage_fixed: true', generate_hooks.command_blocks(writer)['writer'])
        self.assertEqual(generate_hooks.hook_run("      run: 'echo ''quoted'' text'\n"), "echo 'quoted' text")
        self.assertEqual(generate_hooks.hook_run('      run: |-\n        echo body\n\n      stage_fixed: true\n'), 'echo body')
        # I5 a declaring checker with no caller is rejected by the real early
        # preflight in both check/write modes, before rewriting configuration
        # or starting expensive suites. The graph checker shares this owner.
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / 'lefthook.yml'
            path.write_text(rendered)
            for arguments in (['--check'], []):
                errors = io.StringIO()
                with patch.object(generate_hooks, 'HOOKS', path), \
                     patch.object(generate_hooks, 'declared_input_globs', return_value={**declared, 'unwired.py': ('src/**',)}), \
                     contextlib.redirect_stderr(errors):
                    self.assertEqual(generate_hooks.main(arguments), 1)
                self.assertIn('unwired.py declares INPUT_GLOBS', errors.getvalue())
                self.assertEqual(path.read_text(), rendered)
        native = json.loads(subprocess.check_output(['lefthook', 'dump', '--format', 'json'], cwd=ROOT, text=True))
        def names(jobs):
            return [name for job in jobs for name in (names(job['group']['jobs'])
                    if 'group' in job else [job['name']])]
        self.assertCountEqual(names(native['commit-checks']['jobs']),
                              generate_hooks.command_blocks((ROOT / 'lefthook.yml').read_text()).keys())

    def test_parallel_capacity_writers_and_opaque_groups_fail_closed(self):
        # P1 1-4 declared read-only workers with serialized parent -> valid.
        # P2 fifth worker/nested parallel width/root parallel -> refuse; P3 staging writer,
        # missing worker fence, job-label mismatch or inherited group filter
        # -> refuse. No condition can turn an unobserved/skipped job into PASS.
        group = '    - name: readers\n      group:\n        parallel: true\n        jobs:\n'
        jobs = ''.join(leaf(str(i), 'python3 checker.py', indent=10) for i in range(4))
        good = config(group + jobs)
        self.assertEqual(generate_hooks.hook_execution_errors(good), [])
        bad = [config(group + jobs + leaf('fifth', 'python3 checker.py', indent=10)),
               good.replace('  parallel: false', '  parallel: true'),
               good.replace('      group:', '      glob: "never/**"\n      group:', 1),
               good.replace('            tags:', '            stage_fixed: true\n            tags:', 1),
               good.replace('            tags:', '            skip: true\n            tags:', 1),
               good.replace('--read-only ', '', 1),
               good.replace('--hook-label 0 ', '--hook-label wrong ', 1)]
        for source in bad:
            with self.subTest(source=source[:100]):
                self.assertTrue(generate_hooks.hook_execution_errors(source))
        # P4 three serial lanes each containing two jobs have width3; changing
        # only those lanes to parallel has width6 and must fail before launch.
        lanes = ''
        for name in ('a', 'b', 'c'):
            lanes += '          - name: lane-' + name + '\n            group:\n              piped: true\n              jobs:\n'
            lanes += ''.join(leaf(name + str(i), 'python3 checker.py', indent=16) for i in range(2))
        bounded = config(group + lanes)
        self.assertEqual(generate_hooks.hook_execution_errors(bounded), [])
        self.assertTrue(generate_hooks.hook_execution_errors(bounded.replace('              piped: true', '              parallel: true')))

    def test_exact_argument_failure_and_log_transport(self):
        # W1 shell metacharacters, quotes, empty and Unicode file arguments ->
        # exact argv, never code. W2 success -> concise progress + retained log;
        # W3 child23 -> exact23 and visible failure log; trailing && never runs.
        for status in (0, 23):
            task, root, env = self.fixture()
            output = task.directory / 'arguments.json'
            env['PROBE_OUTPUT'] = str(output)
            code = ('import json,os,sys;json.dump(sys.argv[1:],open(os.environ["PROBE_OUTPUT"],"w"));'
                    'print("private-verbose-child-log");sys.exit(' + str(status) + ')')
            body = shlex.join([sys.executable, '-c', code]) + ' "$@" && touch target/dependent'
            (root / 'target').mkdir()
            arguments = ['file with spaces', "file'quote", '$(touch forbidden);`literal`', '', 'ümlaut']
            result = self.worker(task, root, env, body, args=arguments)
            self.assertEqual(result.returncode, status, result.stderr + result.stdout)
            self.assertEqual(json.loads(output.read_text()), arguments)
            self.assertFalse((root / 'forbidden').exists())
            self.assertEqual((root / 'target/dependent').exists(), status == 0)
            observed = ci_performance.hook_observations(task.directory)
            self.assertEqual(observed['rows'][0]['exit_code'], status)
            self.assertEqual(observed['unfinished'], 0)
            self.assertEqual('private-verbose-child-log' in result.stdout, status != 0)
            self.assertIn('[start] fixture', result.stdout)
            self.assertIsNotNone(observed['first_failure_seconds'] if status else observed['rows'][0]['duration_seconds'])
            task.finish(status)

    def test_subject_mutation_and_missing_core_observation_never_pass(self):
        # F1 read-only job changes tracked/working/index bytes -> reject even
        # when child0. F2 explicit serial writer -> retain original0. F3 initial
        # record fails -> no child. F4 final record failure rejects0 but cannot
        # overwrite child23. These use the original atomic writer and source fence.
        for readonly in (True, False):
            task, root, env = self.fixture()
            result = self.worker(task, root, env, 'printf changed > source', readonly=readonly)
            self.assertEqual(result.returncode, int(readonly), result.stderr)
            row = ci_performance.hook_observations(task.directory)['rows'][0]
            if readonly:
                self.assertFalse(row['source_unchanged'])
            task.finish(result.returncode)
        import formal_run_evidence
        task, root, env = self.fixture()
        real_write = formal_run_evidence.write_record
        for phase in ('initial', 'terminal'):
            for status in (0, 23):
                count = []
                def write(path, value):
                    if value['status'] == ('running' if phase == 'initial' else ('passed' if status == 0 else 'failed')):
                        raise OSError('controlled storage failure')
                    real_write(path, value)
                with patch.dict(os.environ, env, clear=True), patch.object(Path, 'cwd', return_value=root), \
                        patch.object(formal_run_evidence, 'write_record', side_effect=write), \
                        patch.object(parallel_check.deadline, 'run', side_effect=lambda *a, **k: count.append(1) or status), \
                        contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    args = parallel_check.parser().parse_args(['--hook-label', 'fault', '--timeout', '1', '--shell', 'true'])
                    if phase == 'initial':
                        with self.assertRaises(OSError):
                            parallel_check.observe_hook(args, [])
                        self.assertEqual(count, [])
                    else:
                        self.assertEqual(parallel_check.observe_hook(args, []), status or 74)
        task.finish(1)

    def test_actual_git_hook_parallel_barrier_selection_and_terminal_lifecycle(self):
        # G1 both real readers must acknowledge one another before completion:
        # serial execution cannot pass. G2 the writer runs after all readers,
        # stages its actual output, then the next reader sees the new subject.
        # G3 an unmatched glob has no worker record or side effect. G4 child23
        # still drains peers, stops dependent groups, refuses Git and records
        # raw23/first-failure separately from Lefthook's aggregate exit1.
        for fails in (False, True):
            with self.subTest(fails=fails):
                initial, root, env = self.fixture()
                initial.finish(0)
                for key in ('AWAKEN_BUILD_RUN_ID', 'AWAKEN_BUILD_RUN_DIRECTORY', 'AWAKEN_BUILD_INVOCATION_ID'):
                    env.pop(key, None)
                probe = initial.directory.parent / ('probe-' + str(fails))
                probe.mkdir()
                env['PROBE_ROOT'] = str(probe)
                # Imported bytecode is derived output, as in the real checkout.
                self.private_ci(root)
                barrier = '''import os,pathlib,sys,time
p=pathlib.Path(os.environ['PROBE_ROOT']);name,other=sys.argv[1:3]
(p/(name+'.ready')).write_text('ready')
until=time.monotonic()+5
# awaken-allow(wait): the other actual worker acknowledges the shared barrier.
while not (p/(other+'.ready')).exists() and time.monotonic()<until: time.sleep(.01)
assert (p/(other+'.ready')).exists(), 'readers must overlap'
(p/(name+'.done')).write_text('done')
sys.exit(23 if name=='alpha' and sys.argv[3]=='fail' else 0)
'''
                reader = lambda name, other: shlex.join([sys.executable, '-c', barrier, name, other,
                                                          'fail' if fails else 'pass'])
                arguments = ('import os,json,sys,pathlib,subprocess;'
                             'assert "LEFTHOOK_CONFIG" not in os.environ;'
                             'assert "AWAKEN_HOOK_PARENT_CONFIG_SET" not in os.environ;'
                             'assert "execution_info" in json.loads(subprocess.check_output(["lefthook","dump","--format","json"],text=True))["output"];'
                             'json.dump(sys.argv[1:],open(pathlib.Path(os.environ["PROBE_ROOT"])/"args.json","w"))')
                writer = ('import os,pathlib,sys;p=pathlib.Path(os.environ["PROBE_ROOT"]);'
                          'assert (p/"alpha.done").exists() and (p/"beta.done").exists();'
                          'pathlib.Path(sys.argv[1]).write_text("formatted");(p/"writer.done").touch()')
                tail = 'import os,pathlib;assert (pathlib.Path(os.environ["PROBE_ROOT"])/"writer.done").exists()'
                jobs = ('    - name: readers\n      group:\n        parallel: true\n        jobs:\n'
                        + '          - name: lane-a\n            group:\n              piped: true\n              jobs:\n'
                        + leaf('arguments', shlex.join([sys.executable, '-c', arguments]) + ' {staged_files}', indent=16, glob='*.py')
                        + leaf('alpha', reader('alpha', 'beta'), indent=16, glob='*.py')
                        + '          - name: lane-b\n            group:\n              piped: true\n              jobs:\n'
                        + leaf('beta', reader('beta', 'alpha'), indent=16, glob='*.py')
                        + leaf('writer', shlex.join([sys.executable, '-c', writer]) + ' {staged_files}', readonly=False, glob='*.py')
                        + '      stage_fixed: true\n'
                        + leaf('tail', shlex.join([sys.executable, '-c', tail]), glob='*.py')
                        + leaf('unmatched', 'touch forbidden', glob='*.rs'))
                (root / 'lefthook.yml').write_text(config(jobs))
                (root / 'lefthook-local.yml').write_text('output: [failure, execution_out, summary, execution_info]\n')
                selected = "file with 'quotes'.py"
                (root / selected).write_text('before')
                intended = (root / 'lefthook.yml').read_text()
                # An older worktree may have installed the shared generic hook.
                # The current script bootstrap must work on its first Git run.
                (root / 'lefthook.yml').write_text('pre-commit:\n  commands:\n    old:\n      run: true\n')
                setup = subprocess.run(['lefthook', 'install', '--force'], cwd=root, env=env,
                                       capture_output=True, text=True, timeout=30)
                self.assertEqual(setup.returncode, 0, setup.stderr + setup.stdout)
                (root / 'lefthook.yml').write_text(intended)
                for command in (['git', 'add', '--', selected],):
                    setup = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True, timeout=30)
                    self.assertEqual(setup.returncode, 0, setup.stderr + setup.stdout)
                result = subprocess.run(['git', '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture feedback'],
                                        cwd=root, env=env, capture_output=True, text=True, timeout=40)
                self.assertEqual(result.returncode, int(fails), result.stdout + result.stderr)
                self.assertEqual(json.loads((probe / 'args.json').read_text()), [selected])
                self.assertEqual((probe / 'writer.done').exists(), not fails)
                self.assertFalse((root / 'forbidden').exists())
                records = [p for p in initial.directory.parent.glob('*/evidence.json') if p.parent != initial.directory]
                self.assertEqual(len(records), 1)
                record = json.loads(records[0].read_text())
                self.assertEqual(record['label'], 'pre-commit')
                self.assertEqual(record['status'], 'failed' if fails else 'passed')
                self.assertTrue(record['workload_digest'])
                self.assertTrue((records[0].parent / 'hook-config.json').is_file())
                self.assertEqual(list(records[0].parent.glob('hook-config-*.yml')), [])
                observed = ci_performance.hook_observations(records[0].parent)
                self.assertEqual(observed['completed'], 3 if fails else 5)
                self.assertEqual(observed['unfinished'], 0)
                if fails:
                    self.assertEqual(next(r for r in observed['rows'] if r['label'] == 'alpha')['exit_code'], 23)
                    self.assertIsNotNone(observed['first_failure_seconds'])
                else:
                    self.assertEqual(subprocess.check_output(['git', 'show', 'HEAD:' + selected], cwd=root, env=env), b'formatted')

    @unittest.skipIf(os.name == 'nt', 'POSIX TERM; native Windows Job cases remain in test_deadline')
    def test_worker_signal_cleans_actual_descendants_and_records_failure(self):
        # S1 actual child PID acknowledged, then TERM ->143 and a failed record;
        # every descendant is reaped by the existing deadline owner.
        task, root, env = self.fixture()
        marker = task.directory / 'child.pid'
        code = 'import os,pathlib,time;pathlib.Path(' + repr(str(marker)) + ').write_text(str(os.getpid()));time.sleep(120)'
        command = [sys.executable, str(ROOT / 'scripts/ci/parallel_check.py'), '--hook-label', 'signal',
                   '--timeout', '30', '--shell', shlex.join([sys.executable, '-c', code]), '--']
        process = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: process.poll() is None and (process.kill(), process.wait()))
        until = time.monotonic() + 5
        # awaken-allow(wait): the actual child PID must exist before cancellation.
        while not marker.exists() and process.poll() is None and time.monotonic() < until:
            time.sleep(.01)
        self.assertTrue(marker.exists())
        child = int(marker.read_text())
        process.terminate()
        process.communicate(timeout=10)
        self.assertEqual(process.returncode, 143)
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)
        self.assertEqual(ci_performance.hook_observations(task.directory)['rows'][0]['exit_code'], 143)
        task.finish(143)

    def test_unfinished_or_corrupt_telemetry_and_changed_config_cannot_bless_a_run(self):
        # R1 valid terminal -> observed. R2 status/exit/duration/run mismatch ->
        # reject. R3 a durable initial record stays unfinished with no invented
        # result. R4 configuration changes after projection -> no terminal PASS.
        task, root, env = self.fixture()
        self.assertEqual(self.worker(task, root, env, 'true').returncode, 0)
        path = next((task.directory / 'hooks').glob('*/observation.json'))
        value = json.loads(path.read_text())
        for fault in ({'status': 'running'}, {'exit_code': 23}, {'duration_seconds': float('nan')},
                      {'run_id': 'foreign'}, {'started_monotonic': float('nan')}):
            path.write_text(json.dumps({**value, **fault}))
            with self.assertRaises(ValueError):
                ci_performance.hook_observations(task.directory)
        path.write_text(json.dumps({**value, 'status': 'running', 'exit_code': None, 'duration_seconds': None}))
        observed = ci_performance.hook_observations(task.directory)
        self.assertEqual((observed['completed'], observed['unfinished']), (0, 1))
        path.write_text(json.dumps(value))
        (root / 'lefthook.yml').write_text('changed configuration')
        (task.directory / 'hook-config.json').write_text(json.dumps({'source_digest': 'previous-configuration'}))
        before = (task.directory / 'evidence.json').read_bytes()
        with self.assertRaisesRegex(ValueError, 'configuration changed'):
            task.finish(0)
        self.assertEqual((task.directory / 'evidence.json').read_bytes(), before)

    def test_missing_context_and_invalid_bridge_arguments_start_no_work(self):
        # B1 no active context ->74, no child. B2 the explicit observation
        # entry accepts only --observe; unsupported argv ->2 before lifecycle.
        task, root, env = self.fixture()
        detached = {key: value for key, value in env.items() if not key.startswith('AWAKEN_BUILD_')}
        result = self.worker(task, root, detached, 'touch forbidden')
        self.assertEqual(result.returncode, 74)
        self.assertFalse((root / 'forbidden').exists())
        for args in ([], ['--unknown'], ['--observe', 'extra']):
            result = subprocess.run([sys.executable, str(ROOT / 'scripts/ci/_native_shell.py'),
                                     str(ROOT / 'scripts/ci/pre-commit/observe.sh'), *args],
                                    cwd=root, env=detached, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(list((task.directory / 'hooks').glob('*')), [])
        task.finish(0)

    @unittest.skipIf(os.name == 'nt', 'POSIX TERM between shell commands')
    def test_parent_term_between_commands_never_records_zero_or_starts_the_gate(self):
        # T1 a real Bash DEBUG boundary after durable begin but before body
        # launch receives TERM. Bash's default EXIT can see0 at this point;
        # the explicit signal owner must return143 and seal failed143, with
        # no checker observation and no compiled configuration left behind.
        initial, root, env = self.fixture()
        initial.finish(0)
        self.private_ci(root)
        (root / 'lefthook.yml').write_text(config(leaf('unreached', 'touch forbidden')))
        injection = initial.directory.parent / 'term-boundary.sh'
        injection.write_text("trap 'case \"$BASH_COMMAND\" in hook_config=*) kill -TERM \"$$\" ;; esac' DEBUG\n")
        env['BASH_ENV'] = str(injection)
        result = subprocess.run([sys.executable, str(root / 'scripts/ci/_native_shell.py'),
                                 str(root / 'scripts/ci/pre-commit/observe.sh'), '--observe'],
                                cwd=root, env=env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 143, result.stderr + result.stdout)
        records = [p for p in initial.directory.parent.glob('*/evidence.json') if p.parent != initial.directory]
        self.assertEqual(len(records), 1)
        record = json.loads(records[0].read_text())
        self.assertEqual((record['status'], record['exit_code']), ('failed', 143))
        self.assertEqual(list((records[0].parent / 'hooks').glob('*')), [])
        self.assertEqual(list(records[0].parent.glob('hook-config-*.yml')), [])
        self.assertFalse((root / 'forbidden').exists())

    def test_progress_reaches_git_caller_before_the_checker_is_released(self):
        # L1 the actual checker blocks on a test-owned release fact. Its start
        # line must traverse BOTH Lefthook layers while Git is still running.
        # L2 without outer follow, the line is buffered until completion and
        # cannot satisfy this positive control. Cleanup releases/reaps either
        # outcome; the test never treats a timeout as successful feedback.
        initial, root, env = self.fixture()
        initial.finish(0)
        self.private_ci(root)
        probe = initial.directory.parent / 'live-probe'
        probe.mkdir()
        env['PROBE_ROOT'] = str(probe)
        code = '''import os,pathlib,time
p=pathlib.Path(os.environ['PROBE_ROOT']);(p/'started').touch()
until=time.monotonic()+8
# awaken-allow(wait): test-owned release fact keeps the actual checker alive.
while not (p/'release').exists() and time.monotonic()<until: time.sleep(.01)
assert (p/'release').exists()
'''
        (root / 'lefthook.yml').write_text(config(leaf('blocked', shlex.join([sys.executable, '-c', code]))))
        setup = subprocess.run(['lefthook', 'install', '--force'], cwd=root, env=env,
                               capture_output=True, text=True, timeout=30)
        self.assertEqual(setup.returncode, 0, setup.stderr)
        (root / 'live-input.txt').write_text('selected staged input')
        subprocess.run(['git', 'add', '--', 'live-input.txt'], cwd=root, env=env, check=True)
        process = subprocess.Popen(['git', '-c', 'commit.gpgsign=false', 'commit', '--allow-empty', '-m', 'live feedback'],
                                   cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, bufsize=1)
        self.addCleanup(lambda: process.poll() is None and (process.kill(), process.wait()))
        visible, output = threading.Event(), []
        def consume():
            for line in process.stdout:
                output.append(line)
                if '[start] blocked' in line:
                    visible.set()
        reader = threading.Thread(target=consume, daemon=True)
        reader.start()
        try:
            observed = visible.wait(timeout=5)
            until = time.monotonic() + 2
            # awaken-allow(wait): the real checker's start fact precedes release.
            while not (probe/'started').exists() and process.poll() is None and time.monotonic()<until:
                time.sleep(.01)
            running = process.poll() is None and (probe/'started').exists()
        finally:
            (probe/'release').touch()
            process.wait(timeout=15)
            reader.join(timeout=2)
            process.stdout.close()
        self.assertTrue(observed and running, 'live progress was buffered: '+''.join(output))
        self.assertEqual(process.returncode, 0, ''.join(output))


if __name__ == '__main__':
    main()
