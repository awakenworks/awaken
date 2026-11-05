"""Resolve the shell before native Windows process creation can select WSL."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


# This module transports commands. The hook bootstrap invokes its self-tests
# and declares their inputs in _lefthook_fitness.py; transport calls must not
# replace the input declarations of the commands they launch.


def bash_executable(*, windows=None, environment=None, find=shutil.which):
    windows = os.name == "nt" if windows is None else windows
    environment = os.environ if environment is None else environment
    if not windows:
        return find("bash") or "bash"
    configured = environment.get("AWAKEN_NATIVE_BASH")
    if configured:
        path = Path(configured)
        if not path.is_absolute() or not path.is_file():
            raise RuntimeError("AWAKEN_NATIVE_BASH must name an existing absolute native Bash executable")
        return str(path)
    git = find("git")
    if git:
        # Git's cmd/, mingw64/bin/ and usr/bin/ entrypoints share the same
        # installation root. Never let CreateProcess resolve bare `bash`, which
        # can choose System32's WSL launcher before the caller's PATH entries.
        for parent in Path(git).resolve().parents:
            # The bin/ launcher also rewrites PATH. The actual MSYS executable
            # preserves a caller-selected Git/shim and avoids another process.
            for relative in ("usr/bin/bash.exe", "bin/bash.exe"):
                candidate = parent / relative
                if candidate.is_file():
                    return str(candidate)
    raise RuntimeError("Set AWAKEN_NATIVE_BASH to the native Git Bash executable")


def self_test():
    # S1 Git cmd or mingw entrypoint + spaces -> same absolute native shell;
    # S2 explicit shell -> exact path; S3 missing/relative override or Git ->
    # fail before launch, never a WSL fallback; S4 POSIX retains PATH lookup.
    with tempfile.TemporaryDirectory(prefix="native shell ") as directory:
        root = Path(directory)
        shell = root / "bin/bash.exe"
        shell.parent.mkdir()
        shell.touch()
        for relative in ("cmd/git.exe", "mingw64/bin/git.exe"):
            git = root / relative
            git.parent.mkdir(parents=True, exist_ok=True)
            git.touch()
            assert bash_executable(windows=True, environment={}, find=lambda _: str(git)) == str(shell)
        direct = root / "usr/bin/bash.exe"
        direct.parent.mkdir(parents=True)
        direct.touch()
        assert bash_executable(windows=True, environment={}, find=lambda _: str(git)) == str(direct)
        assert bash_executable(windows=True, environment={"AWAKEN_NATIVE_BASH": str(shell)}) == str(shell)
        for environment in ({}, {"AWAKEN_NATIVE_BASH": "relative/bash.exe"}, {"AWAKEN_NATIVE_BASH": str(root / "missing.exe")}):
            try:
                bash_executable(windows=True, environment=environment, find=lambda _: None)
            except RuntimeError:
                pass
            else:
                raise AssertionError("missing native shell cannot fall back to WSL")
        assert bash_executable(windows=False, environment={}, find=lambda _: "/usr/bin/bash") == "/usr/bin/bash"


def shell_environment(shell, *, windows=None, environment=None):
    windows = os.name == "nt" if windows is None else windows
    environment = os.environ if environment is None else environment
    child = dict(environment)
    if windows:
        # Nested executable scripts use /usr/bin/env bash. Keep Git's native
        # shell and coreutils ahead of System32's WSL launcher in the child only.
        directory = Path(shell).resolve().parent
        paths = [directory]
        tools = directory.parent / "usr/bin"
        if tools.is_dir() and tools != directory:
            paths.append(tools)
        # Git's bin/git.exe is a launcher for mingw64/bin/git.exe. Every
        # snapshot runs several Git commands; avoid that extra process while
        # retaining the same installation and the caller's parent PATH.
        installation = directory.parent
        if installation.name == "usr":
            installation = installation.parent
        native_git = installation / "mingw64/bin"
        if (native_git / "git.exe").is_file():
            selected = shutil.which("git", path=child.get("PATH", ""))
            entries = (installation / "bin/git.exe", installation / "cmd/git.exe",
                       native_git / "git.exe")
            if selected and Path(selected).resolve() in {entry.resolve() for entry in entries}:
                paths.insert(0, native_git)
            elif tools.is_dir():
                # Keep a caller-selected external Git/shim authoritative. Git's
                # usr/bin supplies nested Bash without bin/git.exe shadowing it.
                paths = [tools]
        previous = child.get("PATH", "")
        child["PATH"] = ";".join([*(str(path) for path in paths), previous])
    return child


def windows_shell_command(command):
    # MSYS parses the raw Windows command line with POSIX escaping; Python's
    # default CRT quoting loses literal quotes, empty args and trailing slashes.
    return " ".join('"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'
                    for value in command)


# The two native Rust launchers use the same MSYS argv encoding as above.
RUST_WINDOWS_SHELL_ARGUMENT = r'''
#[cfg(windows)]
fn windows_shell_argument(argument: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let mut quoted = vec![34u16];
    for value in argument.encode_wide() {
        if value == 34 || value == 92 { quoted.push(92); }
        quoted.push(value);
    }
    quoted.push(34);
    std::ffi::OsString::from_wide(&quoted)
}
'''


def main(argv):
    if argv == ['--lefthook-pre-commit']:
        from _native_lefthook import run_pre_commit
        return run_pre_commit()
    if argv[:1] == ["--windows-job-child"]:
        if os.name != "nt" or len(argv) < 4 or argv[2] != "--":
            return 2
        from _windows_job import attach_current_process
        attach_current_process(argv[1])
        shell = bash_executable()
        return subprocess.call(windows_shell_command([shell, "--", *argv[3:]]),
                               env=shell_environment(shell))
    if argv == ["--self-test"]:
        self_test()
        from _native_shell_tests import run_tests
        return run_tests()
    if not argv or argv[0].startswith("-"):
        print("usage: _native_shell.py SCRIPT [ARG ...] | --self-test", file=sys.stderr)
        return 2
    shell = bash_executable()
    environment = shell_environment(shell)
    command = [shell, "--", *argv]
    if os.name != "nt":
        os.execvpe(shell, command, environment)
    from _windows_job import WindowsJob
    # Use the existing join-before-launch Job mechanism; the parent alone owns
    # the Job handle so cancellation cannot strand native Bash descendants.
    with WindowsJob() as job:
        child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                                  "--windows-job-child", job.name, "--", *argv],
                                 env=environment)
        try:
            return child.wait()
        except BaseException:
            job.close()
            if child.poll() is None:
                child.kill()
            child.wait()
            raise


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
