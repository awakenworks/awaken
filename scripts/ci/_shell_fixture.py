"""Shell fixture helpers shared by the formal gate tests.

Kept out of the test modules so a test that borrows them does not inherit
another test's whole input closure (generate_hooks.py --script-tests follows
imports), which made every script edit run those tests.
"""

from pathlib import Path
import subprocess
import tempfile


def run_shell_fixture(arguments, **kwargs):
    """Keep multiline shell payloads out of native Windows argv quoting.

    The file contains the exact gate slice. A short source trampoline retains
    its original $0 and positional arguments, so receipt/EXIT behavior remains
    the behavior under test. The file stays outside the fixture's Git source.
    """
    if arguments[1:2] != ["-c"]:
        return subprocess.run(arguments, **kwargs)
    script, *positionals = arguments[2:]
    argv0 = positionals[0] if positionals else arguments[0]
    with tempfile.TemporaryDirectory(prefix="awaken-formal-shell-") as directory:
        entry = Path(directory) / "gate-slice.sh"
        entry.write_text(script, encoding="utf-8", newline="\n")
        return subprocess.run([arguments[0], "-c", 'entry=$1; shift; source "$entry"',
                               argv0, entry.as_posix(), *positionals[1:]], **kwargs)


def nested_shell_function(source: str, name: str) -> str:
    marker = f"  {name}() {{\n"
    body = source.split(marker, 1)[1].split("\n  }", 1)[0]
    return f"{name}() {{\n{body}\n}}"
