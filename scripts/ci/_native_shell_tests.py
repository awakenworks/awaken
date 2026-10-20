"""Native shell launch admission, argument integrity and nested shebang proof."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

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
        for arguments in ([], ["--unknown"]):
            executed = subprocess.run([sys.executable, str(Path(__file__).with_name("_native_shell.py")),
                                       *arguments], capture_output=True, timeout=15)
            self.assertEqual(executed.returncode, 2)
            self.assertIn(b"usage:", executed.stderr)


def run_tests():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(NativeShellTests)
    return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
