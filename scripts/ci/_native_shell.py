"""Resolve the shell before native Windows process creation can select WSL."""
import os
from pathlib import Path
import shutil
import tempfile


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
            for relative in ("bin/bash.exe", "usr/bin/bash.exe"):
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
        assert bash_executable(windows=True, environment={"AWAKEN_NATIVE_BASH": str(shell)}) == str(shell)
        for environment in ({}, {"AWAKEN_NATIVE_BASH": "relative/bash.exe"}, {"AWAKEN_NATIVE_BASH": str(root / "missing.exe")}):
            try:
                bash_executable(windows=True, environment=environment, find=lambda _: None)
            except RuntimeError:
                pass
            else:
                raise AssertionError("missing native shell cannot fall back to WSL")
        assert bash_executable(windows=False, environment={}, find=lambda _: "/usr/bin/bash") == "/usr/bin/bash"
