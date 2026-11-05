"""Native shell launch admission, argument integrity and nested shebang proof."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from _native_shell import bash_executable, shell_environment


class NativeShellTests(unittest.TestCase):
    def test_child_path_does_not_mutate_parent_or_posix_environment(self):
        with tempfile.TemporaryDirectory(prefix="native shell environment ") as name:
            root = Path(name)
            shell = root / "bin/bash.exe"
            shell.parent.mkdir()
            tools = root / "usr/bin"
            tools.mkdir(parents=True)
            original = {"PATH": "original", "KEEP": "yes"}
            child = shell_environment(shell, windows=True, environment=original)
            self.assertEqual(child["PATH"], str(shell.parent) + ";" + str(tools) + ";original")
            self.assertEqual(original, {"PATH": "original", "KEEP": "yes"})
            self.assertEqual(shell_environment(shell, windows=False, environment=original), original)
            self.assertIsNot(child, original)
            native_git = root / "mingw64/bin"
            native_git.mkdir(parents=True)
            (native_git / "git.exe").touch()
            expected = ";".join(map(str, (native_git, shell.parent, tools))) + ";original"
            with mock.patch('_native_shell.shutil.which', return_value=str(root / 'cmd/git.exe')):
                self.assertEqual(shell_environment(shell, windows=True, environment=original)["PATH"], expected)
                self.assertEqual(shell_environment(tools / "bash.exe", windows=True,
                                                   environment=original)["PATH"],
                                 ";".join(map(str, (native_git, tools))) + ";original")
            self.assertEqual(original, {"PATH": "original", "KEEP": "yes"})

    @unittest.skipUnless(os.name == 'nt', 'native Windows child-selected Git')
    def test_formal_git_lookup_uses_child_path_and_preserves_raw_results(self):
        import json
        import shutil
        from contextlib import chdir
        from _native_test_launcher import native_fixture_launcher
        import formal_sources
        original = dict(os.environ)
        with tempfile.TemporaryDirectory(prefix='formal native Git ') as raw:
            root = Path(raw).resolve()
            selected = root / 'relative tools 中文✓'
            selected.mkdir()
            worktree = root / 'other working directory'
            worktree.mkdir()
            shutil.copyfile(native_fixture_launcher(), selected / 'git.exe')
            script = root / 'selected_git.py'
            script.write_text('import json,sys\n'
                              'a=sys.argv[1:]\n'
                              'if "raw-status" in a: raise SystemExit(37)\n'
                              'if "no-match" in a: raise SystemExit(1)\n'
                              'if "grep-error" in a:\n'
                              ' print("controlled-git-error",file=sys.stderr);raise SystemExit(23)\n'
                              'print(json.dumps(a))\n', encoding='utf-8')
            child = dict(original, PATH=selected.name + os.pathsep + original['PATH'],
                         AWAKEN_FIXTURE_PROGRAM=sys.executable,
                         AWAKEN_FIXTURE_SCRIPT=str(script))
            with chdir(root):
                command = formal_sources._git_command(child, '--version')
                self.assertEqual(Path(command[0]), selected / 'git.exe')
                self.assertTrue(Path(command[0]).is_absolute())
                arguments = ['--version', 'with spaces', '中文✓', '']
                output = formal_sources._git_output(worktree, child, *arguments)
                self.assertEqual(json.loads(output),
                                 ['-c', 'core.fsmonitor=false', '-c', 'diff.autoRefreshIndex=false',
                                  '-C', str(worktree), *arguments])
                output = formal_sources._git_grep(worktree, child, ['hit'])
                self.assertEqual(json.loads(output),
                                 ['grep', '--cached', '--no-color', '--full-name', '-I', 'hit'])
                self.assertEqual(formal_sources._git_grep(worktree, child, ['no-match']), b'')
                with self.assertRaisesRegex(formal_sources.SourceError, 'controlled-git-error'):
                    formal_sources._git_grep(worktree, child, ['grep-error'])
                with self.assertRaises(subprocess.CalledProcessError) as failed:
                    formal_sources._git_output(worktree, child, 'raw-status')
                self.assertEqual(failed.exception.returncode, 37)
                missing = root / 'missing Git'
                missing.mkdir()
                with self.assertRaises(FileNotFoundError):
                    formal_sources._git_command(dict(child, PATH=str(missing)), '--version')
        self.assertEqual(dict(os.environ), original)

    def test_real_nested_shebang_preserves_arguments_and_failure(self):
        with tempfile.TemporaryDirectory(prefix="native Bash argv ") as name:
            root = Path(name)
            result = root / "result.json"
            marker = root / "must-not-exist"
            nested = root / "nested.sh"
            nested.write_text("#!/usr/bin/env bash\nprintf '%s\\0' \"$@\" >\"$OUTPUT\"\nexit 23\n",
                              encoding="utf-8", newline="\n")
            nested.chmod(0o755)
            entry = root / "entry.sh"
            entry.write_text('#!/usr/bin/env bash\nexec "$NESTED" "$@"\n',
                             encoding="utf-8", newline="\n")
            env = dict(os.environ, OUTPUT=result.as_posix(), NESTED=nested.as_posix())
            if os.name == "nt":
                # Simulate a native host selecting WSL before Git's usr/bin.
                env["PATH"] = str(Path(os.environ["SystemRoot"]) / "System32")
                env["AWAKEN_NATIVE_BASH"] = bash_executable()
            arguments = ["with spaces", "a;b", "$(touch " + marker.as_posix() + ")", 'quote"value', "", "trailing\\", 'quote"with spaces']
            executed = subprocess.run([sys.executable, str(Path(__file__).with_name("_native_shell.py")),
                                       entry.as_posix(), *arguments], env=env, capture_output=True,
                                      timeout=30)
            self.assertEqual(executed.returncode, 23, executed.stderr.decode(errors="replace"))
            self.assertEqual(result.read_bytes().decode().split("\0")[:-1], arguments)
            self.assertFalse(marker.exists())

    def test_invalid_entry_fails_before_launch(self):
        for arguments in ([], ["--unknown"], ["--lefthook-pre-commit", "--skip"]):
            executed = subprocess.run([sys.executable, str(Path(__file__).with_name("_native_shell.py")),
                                       *arguments], capture_output=True, timeout=15)
            self.assertEqual(executed.returncode, 2)
            self.assertIn(b"usage:", executed.stderr)

    @unittest.skipUnless(os.name == 'nt', 'native Windows fixture shell argv')
    def test_native_fixture_shell_preserves_script_and_literal_arguments(self):
        from _native_test_launcher import native_fixture_launcher
        with tempfile.TemporaryDirectory(prefix='native fixture shell ') as raw:
            root = Path(raw)
            output, marker = root / 'argv.bin', root / 'must-not-exist'
            script = root / "fixture '中文' shell.sh"
            script.write_text('printf \'%s\\0\' "$@" > "$OUTPUT"\nexit 23\n',
                              encoding='utf-8', newline='\n')
            arguments = ['with spaces', "single'quote", 'double"quote', '', 'trailing\\',
                         '中文✓', '$(touch ' + marker.as_posix() + ')', '`literal`']
            env = dict(os.environ, AWAKEN_FIXTURE_PROGRAM=bash_executable(),
                       AWAKEN_FIXTURE_SCRIPT=str(script), OUTPUT=str(output))
            executed = subprocess.run([str(native_fixture_launcher()), *arguments], env=env,
                                      capture_output=True, timeout=30)
            self.assertEqual(executed.returncode, 23, executed.stderr.decode(errors='replace'))
            self.assertEqual(output.read_bytes().decode('utf-8').split('\0')[:-1], arguments)
            self.assertFalse(marker.exists())

    @unittest.skipUnless(os.name == 'nt', 'native Windows caller-selected Git shim')
    def test_nested_shell_preserves_the_callers_git_shim(self):
        import json
        import shutil
        from _native_test_launcher import native_fixture_launcher
        with tempfile.TemporaryDirectory(prefix='native custom git ') as raw:
            root = Path(raw)
            shim = root / 'selected-tools'
            shim.mkdir()
            shutil.copyfile(native_fixture_launcher(), shim / 'git.exe')
            output = root / 'git-argv.json'
            program = root / 'git.py'
            program.write_text('import json,os,sys\nfrom pathlib import Path\n'
                               'Path(os.environ["OUTPUT"]).write_text(json.dumps(sys.argv[1:]))\n'
                               'raise SystemExit(23)\n', encoding='utf-8')
            entry = root / 'entry.sh'
            entry.write_text('git "$@"\n', encoding='utf-8', newline='\n')
            env = dict(os.environ, PATH=str(shim) + os.pathsep + os.environ['PATH'],
                       AWAKEN_NATIVE_BASH=bash_executable(), AWAKEN_FIXTURE_PROGRAM=sys.executable,
                       AWAKEN_FIXTURE_SCRIPT=str(program), OUTPUT=str(output))
            arguments = ['with spaces', 'quote"value', '中文✓', '']
            result = subprocess.run([sys.executable, str(Path(__file__).with_name('_native_shell.py')),
                                     str(entry), *arguments], env=env, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 23, result.stderr.decode(errors='replace'))
            self.assertEqual(json.loads(output.read_text()), arguments)


def run_tests():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(NativeShellTests)
    return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
