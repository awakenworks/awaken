"""Controlled late-launch regressions; every process and path is fixture-owned."""
from contextlib import redirect_stdout
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from threading import Event
import time
from types import SimpleNamespace
from unittest import mock


def admission_refusals(case):
    with tempfile.TemporaryDirectory(prefix='execution-admission-') as raw:
        base = Path(raw).resolve()
        case.assertEqual(base.parent, Path(tempfile.gettempdir()).resolve())
        for mode, permit in (('eof', b''), ('wrong-permit', b'\x00'), ('source-drift', b'\x01')):
            root = base/mode
            root.mkdir()
            script, report = root/'fixture.py', root/'result.json'
            script.write_text('''from pathlib import Path
import sys,unittest
sys.path.insert(0,SCRIPTS)
from _execution_self_tests import run_case
root=Path(ROOT)
class Subject(unittest.TestCase):
 def setUp(self): (root/'setup.started').write_text('setup ran')
 def test_body(self): (root/'body.started').write_text('body ran')
raise SystemExit(run_case(Path(__file__),Subject('test_body'),root/'result.json',admitted=True))
'''.replace('SCRIPTS', repr(str(Path(__file__).parent))).replace('ROOT', repr(str(root))),
                              encoding='utf-8', newline='\n')
            process = subprocess.Popen([sys.executable, '-B', '-X', 'utf8', str(script)],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                ready = json.loads(process.stdout.readline())
                case.assertEqual(ready['case'], '__main__.Subject.test_body')
                if mode == 'source-drift':
                    with script.open('a', encoding='utf-8') as source:
                        source.write('\n# changed after readiness\n')
                _, errors = process.communicate(input=permit, timeout=5)
                case.assertEqual(process.returncode, 1, errors.decode('utf-8', errors='replace'))
                case.assertFalse((root/'setup.started').exists())
                case.assertFalse((root/'body.started').exists())
                case.assertFalse(report.exists())
                if mode == 'source-drift':
                    case.assertIn(b'source changed before its admission', errors)
            finally:
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=5)
                for stream in (process.stdin, process.stdout, process.stderr):
                    stream.close()
            print(f'worker admission {mode}: raw1, no setup/body/report, owned process exited', flush=True)


def cancellation_barriers(case):
    import _execution_self_tests as workers
    import _windows_job
    from _build_observation_fixture import synthetic_build_environment
    with tempfile.TemporaryDirectory(prefix='execution-launch-cancel-') as raw:
        root = Path(raw).resolve()
        case.assertEqual(root.parent, Path(tempfile.gettempdir()).resolve())
        control_code = ('from pathlib import Path;import sys,msvcrt;'
                        'f=open(sys.argv[1],"wb");f.write(b"x");f.flush();f.seek(0);'
                        'msvcrt.locking(f.fileno(),msvcrt.LK_NBLCK,1);'
                        'Path(sys.argv[2]).write_text("ready");sys.stdin.buffer.read(1);f.close()')
        control = subprocess.Popen([sys.executable, '-B', '-c', control_code,
                                    str(root/'control.lock'), str(root/'control.ready')], stdin=subprocess.PIPE)
        try:
            until = time.monotonic()+5
            while not (root/'control.ready').exists():
                case.assertLess(time.monotonic(), until)
                # awaken-allow(wait): the owned control acknowledges its live exclusive file lock.
                time.sleep(.01)
            for mode in ('before-popen', 'attach-handle-open'):
                _barrier(case, root/mode, workers, _windows_job, synthetic_build_environment)
                case.assertIsNone(control.poll())
                with case.assertRaises(PermissionError):
                    (root/'control.lock').rename(root/'control-must-stay-locked')
        finally:
            if control.poll() is None:
                control.stdin.write(b'x')
                control.stdin.flush()
            control.stdin.close()
            control.wait(timeout=5)
        case.assertEqual(control.returncode, 0)


def _barrier(case, root, workers, jobs, environment):
    root.mkdir()
    scripts = Path(__file__).parent
    fixture = root/'fixture.py'
    fixture.write_text('''from pathlib import Path
import argparse,sys,unittest
sys.path.insert(0, SCRIPTS)
from _execution_self_tests import run_case
class ExecutionTests(unittest.TestCase):
 def test_late(self): Path(MARKER).write_text('case body ran')
 def test_boom(self): pass
p=argparse.ArgumentParser(allow_abbrev=False)
p.add_argument('--self-test',action='store_true')
p.add_argument('--self-test-case')
p.add_argument('--self-test-report',type=Path)
p.add_argument('--self-test-admitted',action='store_true')
a=p.parse_args()
raise SystemExit(run_case(Path(__file__),ExecutionTests(a.self_test_case),
                         a.self_test_report,admitted=a.self_test_admitted))
'''.replace('SCRIPTS', repr(str(scripts))).replace('MARKER', repr(str(root/'case.started'))),
                       encoding='utf-8', newline='\n')
    for name in ('deadline.py', '_windows_job.py'):
        shutil.copy2(scripts/name, root/name)
    if root.name == 'attach-handle-open':
        (root/'deadline.py').write_text('''from pathlib import Path
from ctypes import wintypes
import ctypes,json,os,subprocess,sys,time
root=Path(ROOT)
k=ctypes.WinDLL('kernel32',use_last_error=True)
k.OpenJobObjectW.argtypes=[wintypes.DWORD,wintypes.BOOL,wintypes.LPCWSTR];k.OpenJobObjectW.restype=wintypes.HANDLE
k.GetCurrentProcess.restype=wintypes.HANDLE
k.AssignProcessToJobObject.argtypes=[wintypes.HANDLE,wintypes.HANDLE]
k.CloseHandle.argtypes=[wintypes.HANDLE]
h=k.OpenJobObjectW(1,False,sys.argv[2]);assert h
(root/'attach.opened').write_text(str(os.getpid()))
until=time.monotonic()+5
while not (root/'parent.closed').exists():
 if time.monotonic()>=until: raise RuntimeError('parent never closed its Job before waiting')
 time.sleep(.01)
assigned=bool(k.AssignProcessToJobObject(h,k.GetCurrentProcess()))
(root/'assigned.json').write_text(json.dumps({'assigned':assigned,'after_parent_close':True}))
k.CloseHandle(h)
if not assigned:raise SystemExit(1)
raise SystemExit(subprocess.call(sys.argv[4:]))
'''.replace('ROOT', repr(str(root))), encoding='utf-8', newline='\n')
    sealed, parked, closed = Event(), Event(), Event()
    processes, shutdown_checks = [], []
    original_open, original_popen = Path.open, subprocess.Popen
    original_job, original_pool = jobs.WindowsJob, workers.ThreadPoolExecutor

    class TrackingJob(original_job):
        def close(self):
            had_handle = bool(self.handle)
            super().close()
            if had_handle:
                (root/'parent.closed').write_text('closed')
                closed.set()

    class TrackingPool(original_pool):
        def shutdown(self, *args, **kwargs):
            shutdown_checks.append(closed.is_set())
            return super().shutdown(*args, **kwargs)

    def open_path(path, *args, **kwargs):
        if path.name == 'test_late.log' and root.name == 'before-popen':
            parked.set()
            case.assertTrue(sealed.wait(5), 'cleanup never sealed new launches')
        if path.name == 'test_boom.log':
            until = time.monotonic()+5
            while not (parked.is_set() if root.name == 'before-popen' else (root/'attach.opened').exists()):
                case.assertLess(time.monotonic(), until)
                # awaken-allow(wait): the second lane waits for the exact controlled launch barrier.
                time.sleep(.01)
            raise OSError('controlled worker log writer fault')
        return original_open(path, *args, **kwargs)

    def popen(*args, **kwargs):
        process = original_popen(*args, **kwargs)
        processes.append(process)  # retained native handles, never PID-only cleanup
        return process

    names = ('test_late', 'test_boom')
    descriptors = [SimpleNamespace(_testMethodName=name,
                    id=lambda name=name: '__main__.ExecutionTests.'+name) for name in names]
    with mock.patch.dict('os.environ', environment(root), clear=True), \
         mock.patch.object(workers, 'Event', return_value=sealed), \
         mock.patch.object(jobs, 'WindowsJob', TrackingJob), \
         mock.patch.object(workers, 'ThreadPoolExecutor', TrackingPool), \
         mock.patch.object(Path, 'open', open_path), \
         mock.patch.object(subprocess, 'Popen', popen), \
         (root/'parent.log').open('w', encoding='utf-8') as log, redirect_stdout(log):
        with case.assertRaisesRegex(OSError, 'controlled worker log writer fault'):
            workers.run_cases(fixture, descriptors)
    case.assertTrue(sealed.is_set())
    case.assertTrue(closed.is_set())
    case.assertEqual(shutdown_checks, [True])
    case.assertFalse((root/'case.started').exists())
    case.assertTrue(all(process.poll() is not None for process in processes))
    if root.name == 'before-popen':
        case.assertEqual(processes, [])
    else:
        case.assertEqual(len(processes), 1)
        case.assertEqual(json.loads((root/'assigned.json').read_bytes()),
                         {'assigned': True, 'after_parent_close': True})
    print(f'late-launch barrier {root.name}: no case body; '
          f'{len(processes)} native process handles exited; Job closed before pool shutdown', flush=True)
