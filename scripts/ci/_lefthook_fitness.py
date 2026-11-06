"""Process tests of the pre-push bootstrap (qualify.sh) in real Git and Lefthook.

The static hook-shape rule (`hook_entry_errors`) is owned by
`generate_hooks.py --check`; this module proves the shape behaves.
"""

from __future__ import annotations

import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

from _native_shell import bash_executable
from generate_hooks import hook_entry_errors

INPUT_GLOBS: tuple[str, ...] = (
    "lefthook.yml",
    "scripts/ci/pre-push/qualify.sh",
    "scripts/ci/pre-commit/observe.sh",
    "scripts/ci/_build_observation.sh",
    "scripts/ci/parallel_check.py",
    "scripts/ci/ci_performance.py",
    "scripts/ci/generate_hooks.py",
    "scripts/ci/_lefthook_fitness.py",
    "scripts/ci/_native_shell.py",
    "scripts/ci/_native_shell_tests.py",
    "scripts/ci/_windows_job.py",
    "scripts/ci/deadline.py",
)
HOOK_ENTRY = "scripts/ci/pre-push/qualify.sh"


def self_test(root: Path) -> None:
    subprocess.run([sys.executable, str(root / "scripts/ci/_native_shell.py"), "--self-test"], check=True)
    hooks = (root / "lefthook.yml").read_text(encoding="utf-8")
    assert not hook_entry_errors(hooks), "E1 mandatory script entry"
    for old, new in (("source_dir: scripts/ci", "source_dir: elsewhere"),
                     ("qualify.sh:", "absent.sh:"), ("args: --qualify", "args: --other"),
                     ("runner: python3 scripts/ci/_native_shell.py", "runner: bash"),
                     ("pre-push:\n", "pre-push:\n  skip: true\n"),
                     ("push-qualification:", "unused-qualification:"),
                     ("  scripts:\n    qualify.sh:", "  commands:\n    qualify.sh:")):
        assert old in hooks and hook_entry_errors(hooks.replace(old, new, 1)), "E2 unsafe bootstrap"
    before, qualification = hooks.split("\npush-qualification:", 1)
    section = re.search(r"(?ms)^push-qualification:\n(.*?)(?=^\S|\Z)", hooks).group(1)
    for command in re.findall(r"(?m)^      run: (.+)$", section):
        assert hook_entry_errors(before + "\npush-qualification:" + qualification.replace(
            "run: " + command, "run: true", 1)), "E3 mandatory qualification cannot become empty or partial"
    payload = b"HEAD abc refs/heads/main def\nrefs/heads/next ghi refs/heads/next jkl\n"
    with tempfile.TemporaryDirectory(prefix="hook protocol ") as directory:
        fixture = Path(directory)
        binary = fixture / "lefthook"
        binary.write_text('#!/bin/sh\nprintf "%s\\0" "$@" >> "$FIXTURE_ARGS"\n'
                          'cat > "$FIXTURE_STDIN"\nexit "$FIXTURE_STATUS"\n', encoding="utf-8")
        binary.chmod(0o755)
        args_file, stdin_file = fixture / "args", fixture / "stdin"
        environment = {**os.environ, "PATH": str(fixture) + os.pathsep + os.environ["PATH"],
                       "FIXTURE_ARGS": args_file.as_posix(), "FIXTURE_STDIN": stdin_file.as_posix()}
        for status in (0, 37):
            args_file.unlink(missing_ok=True)
            environment["FIXTURE_STATUS"] = str(status)
            result = subprocess.run([bash_executable(), str(root / HOOK_ENTRY), "--qualify"],
                                    env=environment, input=payload, capture_output=True, timeout=15)
            assert result.returncode == status, result.stderr
            assert args_file.read_bytes().split(b"\0")[:-1] == [b"run", b"push-qualification"]
            assert stdin_file.read_bytes() == payload
        for arguments in ((), ("run",), ("--qualif",), ("--qualify", "--watch"),
                          ("--awaken-gate-argument-probe",), ("origin", "remote with spaces")):
            before = args_file.read_bytes()
            result = subprocess.run([bash_executable(), str(root / HOOK_ENTRY), *arguments],
                                    env=environment, input=b"", capture_output=True, timeout=15)
            assert result.returncode == 2 and b"usage: qualify.sh" in result.stderr, arguments
            assert args_file.read_bytes() == before, "refused arguments cannot execute a child"
    print("hook bootstrap: 8 process cases passed (qualification, failure and refusals)")


def _fixture_git() -> str:
    executable = shutil.which("git")
    if not executable:
        raise RuntimeError("Git is required for hook integration")
    if os.name != "nt":
        return executable
    # A Git hook prepends libexec/git-core to PATH. Its internal executable is
    # not the standalone Windows entry point: thinning PATH around it loses
    # the runtime setup performed by cmd/git.exe and can crash a local push.
    for parent in Path(executable).resolve().parents[:4]:
        launcher = parent / "cmd" / "git.exe"
        if launcher.is_file():
            return str(launcher)
    raise RuntimeError("Hook integration requires the public Git for Windows cmd/git.exe launcher")


def _fixture_environment() -> dict[str, str]:
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("GIT_", "LEFTHOOK_")) and key != "LEFTHOOK"}
    environment.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
    return environment


def integration_test_in_git_hook(root: Path) -> None:
    """Exercise the fixture from a real hook, including Git's inherited PATH."""
    environment, git_binary = _fixture_environment(), _fixture_git()
    parent = root / "target" / "hook-regression"
    parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nested Git hook ", dir=parent) as directory:
        fixture = Path(directory).resolve()
        assert fixture.is_relative_to(parent.resolve())
        repo, empty = fixture / "repo", fixture / "empty"
        repo.mkdir(); empty.mkdir()
        for arguments in (("init", "--quiet", "--initial-branch=fixture", "--template=" + str(empty)),
                          ("config", "user.name", "Hook fixture"),
                          ("config", "user.email", "hook@example.invalid"),
                          ("config", "commit.gpgsign", "false")):
            subprocess.run([git_binary, *arguments], cwd=repo, env=environment,
                           input=b"", capture_output=True, check=True, timeout=30)
        # Loading this same file under another name calls only the eight-case
        # fixture, without recursively entering this outer regression wrapper.
        probe = ("import runpy,sys;from pathlib import Path;"
                 f"sys.path.insert(0,{str(root / 'scripts/ci')!r});"
                 f"runpy.run_path({str(Path(__file__).resolve())!r},run_name='hook_fixture')"
                 f"['integration_test'](Path({str(root)!r}))")
        hook = repo / ".git" / "hooks" / "pre-commit"
        hook.parent.mkdir()
        hook.write_text("#!/bin/sh\nexec " + shlex.join([Path(sys.executable).as_posix(), "-X", "utf8", "-c", probe]) + "\n",
                        encoding="utf-8", newline="\n")
        hook.chmod(0o755)
        # Use the same native Bash -> Git -> hook topology as commit.sh. This
        # also covers its inherited MinGW environment when CI starts in PowerShell.
        entry = fixture / "invoke.sh"
        entry.write_text("#!/usr/bin/env bash\nexec " + shlex.join([
            Path(git_binary).as_posix(), "commit", "--quiet", "--allow-empty", "-m", "nested hook fixture"
        ]) + "\n", encoding="utf-8", newline="\n")
        command = [sys.executable, str(root / "scripts/ci/deadline.py"), "180", "--",
                   sys.executable, str(root / "scripts/ci/_native_shell.py"), str(entry)]
        result = subprocess.run(command, cwd=repo, env=environment,
                                input=b"", capture_output=True, timeout=195)
        if result.returncode:
            raise AssertionError(("real Git hook integration", result.returncode,
                                  result.stdout.decode(errors="replace"), result.stderr.decode(errors="replace")))
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        sys.stderr.buffer.write(result.stderr)
        sys.stderr.buffer.flush()
    print("hook nesting: all integration cases passed from an actual Git hook")


def integration_test(root: Path) -> None:
    """Actual Git/Lefthook; no real remotes, indexes, hooks or global config."""
    binary = shutil.which("lefthook")
    if not binary:
        raise RuntimeError("Lefthook is required for the hook integration test")
    environment, git_binary = _fixture_environment(), _fixture_git()
    if os.name == "nt":
        # A native developer's PATH need not include Git's Unix tools. Keep
        # System32 (and its WSL launcher), Python, Git cmd and Lefthook only.
        environment["AWAKEN_NATIVE_BASH"] = bash_executable()
        tools = [shutil.which("python3"), git_binary, binary]
        assert all(tools), tools
        environment["PATH"] = os.pathsep.join(dict.fromkeys([
            str(Path(os.environ["SystemRoot"]) / "System32"),
            *(str(Path(tool).parent) for tool in tools),
        ]))
    parent = root / "target" / "hook-regression"
    parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="shared hook ", dir=parent) as directory:
        fixture = Path(directory).resolve()
        assert fixture.is_relative_to(parent.resolve())
        repo, remote, empty = fixture / "repo", fixture / "remote.git", fixture / "empty"
        repo.mkdir(); empty.mkdir()

        def run(command: list[str], *, check: bool = True) -> subprocess.CompletedProcess:
            result = subprocess.run(command, cwd=repo, env=environment, input=b"", capture_output=True, timeout=60)
            if check and result.returncode:
                raise AssertionError((command, result.returncode, result.stderr.decode(errors="replace")))
            return result

        def git(*arguments: str, check: bool = True) -> subprocess.CompletedProcess:
            return run([git_binary, *arguments], check=check)

        git("init", "--quiet", "--initial-branch=tmp", "--template=" + str(empty))
        git("config", "user.name", "Hook fixture"); git("config", "user.email", "hook@example.invalid")
        git("config", "commit.gpgsign", "false")
        git("init", "--bare", "--quiet", "--template=" + str(empty), str(remote))
        git("remote", "add", "origin", str(remote))
        (repo / "capture.py").write_text(
            "import pathlib,sys\np=pathlib.Path('events')\n"
            "p.write_text(p.read_text()+'1' if p.exists() else '1')\n"
            "sys.exit(37 if pathlib.Path('fail').exists() else 0)\n", encoding="utf-8")
        (repo / ".gitignore").write_text("events\nfail\n", encoding="utf-8")
        config, value = repo / "lefthook.yml", repo / "value"
        old = "output: [failure]\npre-push:\n  commands:\n    probe:\n      run: python3 capture.py\n"
        new = ("output: [failure]\nsource_dir: scripts/ci\npre-push:\n  scripts:\n"
               "    qualify.sh:\n      runner: python3 scripts/ci/_native_shell.py\n      args: --qualify\n"
               "push-qualification:\n  commands:\n    probe:\n      run: python3 capture.py\n")
        config.write_text(old, encoding="utf-8"); value.write_text("base", encoding="utf-8")
        git("add", "."); git("commit", "--quiet", "-m", "base"); git("push", "--quiet", "origin", "HEAD:main")
        git("config", "branch.tmp.remote", "origin"); git("config", "branch.tmp.merge", "refs/heads/tmp")
        git("symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/tmp")
        entry = repo / HOOK_ENTRY
        entry.parent.mkdir(parents=True); entry.write_bytes((root / HOOK_ENTRY).read_bytes())
        for name in ("_native_shell.py", "_windows_job.py"):
            shutil.copyfile(root / "scripts/ci" / name, repo / "scripts/ci" / name)

        def count() -> int:
            return len((repo / "events").read_text()) if (repo / "events").exists() else 0

        def install() -> None:
            run([binary, "install"])

        for label, source, stale in (("stale-head", "HEAD", True), ("stale-named", "tmp", True),
                                     ("fresh-head", "HEAD", False)):
            config.write_text(new, encoding="utf-8"); value.write_text(label, encoding="utf-8")
            git("add", "."); git("commit", "--quiet", "-m", label)
            # Overwrite shared hooks AFTER committing: a prepare-commit-msg
            # auto-install must not mask the first-push bootstrap regression.
            if stale:
                config.write_text(old, encoding="utf-8"); install(); config.write_text(new, encoding="utf-8")
            else:
                install()
            before = count(); git("push", "origin", source + ":main")
            assert count() == before + 1, label
        before = count(); git("push", "origin", "HEAD:main")
        assert count() == before + 1, "up-to-date push still qualifies the checkout"
        git("commit", "--quiet", "--allow-empty", "-m", "empty")
        before = count(); git("push", "origin", "HEAD:main")
        assert count() == before + 1, "empty commit still qualifies the checkout"
        for failure in ("command", "bootstrap", "definition"):
            config.write_text(new, encoding="utf-8"); entry.write_bytes((root / HOOK_ENTRY).read_bytes())
            value.write_text(failure, encoding="utf-8"); git("add", "."); git("commit", "--quiet", "-m", failure)
            if failure == "command": (repo / "fail").touch()
            elif failure == "bootstrap": entry.unlink()
            else: config.write_text(new.replace("push-qualification:", "wrong-qualification:"), encoding="utf-8")
            remote_before = git("ls-remote", "origin", "refs/heads/main").stdout
            before = count(); result = git("push", "origin", "HEAD:main", check=False)
            assert result.returncode != 0, failure
            assert count() == before + int(failure == "command"), failure
            assert remote_before == git("ls-remote", "origin", "refs/heads/main").stdout, failure
            (repo / "fail").unlink(missing_ok=True)
    print("hook integration: 8 real Git cases passed (stale/fresh hooks, empty pushes and failure fences)")


if __name__ == "__main__":
    root = Path(__file__).resolve().parents[2]
    if sys.argv[1:] == ["--integration-test"]:
        integration_test_in_git_hook(root)
    elif sys.argv[1:] == ["--self-test"]:
        self_test(root)
    else:
        print("usage: _lefthook_fitness.py [--self-test|--integration-test]", file=sys.stderr)
        raise SystemExit(2)
