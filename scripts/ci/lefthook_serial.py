#!/usr/bin/env python3
"""Make partially staged pre-commits in different worktrees take turns.

Lefthook 2.1 backs up the unstaged half of a partially staged pre-commit with
``git stash push --message "lefthook auto backup"`` and a patch at
``git rev-parse --git-path info/lefthook-unstaged.patch``; afterwards it applies
that patch and drops the stash it finds by that message. The stash ref and
``info/`` live in the common git dir that every worktree shares, so two
worktrees committing at once restore each other's unstaged hunks and lose their
own. Lefthook has no setting that turns the backup off.

``lefthook.yml`` ``rc:`` sources ``scripts/ci/pre-commit/lefthook-rc.sh`` into
every generated hook, which points the hook's ``LEFTHOOK_BIN`` here. This
wrapper runs the lefthook the hook would have run (the caller's
``LEFTHOOK_BIN``, else ``lefthook`` on PATH, else ``~/.local/bin/lefthook``)
with the same arguments, and returns its exit status or dies of its signal.
Only ``run pre-commit`` with a path that is both staged and unstaged first takes
an exclusive lock in the common git dir, held for the whole lefthook run. A
whole-file commit, any other hook, and a lefthook nested inside a run that
already holds the lock (``observe.sh`` starts one) never wait.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SELF = Path(__file__).resolve()
LOCK_NAME = "awaken-pre-commit.lock"
# The caller's LEFTHOOK_BIN as the rc file found it ("" when it was unset).
PRESERVED = "AWAKEN_LEFTHOOK_BIN"
# Set for lefthook's children while this process holds the lock of that common dir.
HELD = "AWAKEN_PRE_COMMIT_LOCK_HELD"
RECORD_SIZE = 1024
# msvcrt locks a byte range and refuses reads inside it; lock past the record.
WINDOWS_LOCK_OFFSET = 1 << 20


def _git(arguments: list[str], cwd=None, environment=None) -> bytes | None:
    try:
        result = subprocess.run(["git", *arguments], cwd=cwd, env=environment, capture_output=True,
                                stdin=subprocess.DEVNULL)
    except OSError:
        return None
    return result.stdout if result.returncode == 0 else None


def partially_staged(staged: bytes, unstaged: bytes) -> list[str]:
    """Paths in both NUL-separated ``git diff --name-only -z`` listings."""
    decode = lambda output: {name.decode("utf-8", "surrogateescape") for name in output.split(b"\0") if name}
    return sorted(decode(staged) & decode(unstaged))


def partially_staged_paths(cwd=None) -> list[str]:
    staged = _git(["diff", "--cached", "--name-only", "--no-renames", "-z"], cwd)
    unstaged = _git(["diff", "--name-only", "--no-renames", "-z"], cwd)
    return [] if staged is None or unstaged is None else partially_staged(staged, unstaged)


def common_dir(cwd=None, environment=None) -> str | None:
    found = _git(["rev-parse", "--git-common-dir"], cwd, environment)
    if not found:
        return None
    path = Path(found.decode("utf-8", "surrogateescape").strip())
    return os.path.normcase(os.path.realpath(Path(cwd or os.getcwd()) / path))


def _is_wrapper(candidate: str) -> bool:
    if not candidate:
        return False
    path = Path(candidate)
    try:
        return path.name == SELF.name or path.resolve() == SELF or os.path.samefile(path, SELF)
    except OSError:
        return False


def real_lefthook(environment) -> str | None:
    """The hook template's lefthook, never this wrapper or another copy of it."""
    search = environment.get("PATH", os.defpath)
    preserved = environment.get(PRESERVED) or ""
    found = [shutil.which(preserved, path=search) if preserved else None]
    found += [shutil.which("lefthook", path=directory) for directory in search.split(os.pathsep) if directory]
    found.append(shutil.which(str(Path.home() / ".local" / "bin" / "lefthook")))
    return next((path for path in found if path and not _is_wrapper(path)), None)


def child_environment(environment) -> dict[str, str]:
    """Lefthook's environment: the caller's LEFTHOOK_BIN back, the rc's variable gone."""
    child = dict(environment)
    original = child.pop(PRESERVED, None)
    if original:
        child["LEFTHOOK_BIN"] = original
    elif original is not None or _is_wrapper(child.get("LEFTHOOK_BIN", "")):
        child.pop("LEFTHOOK_BIN", None)
    return child


def _primitives():
    """``(try_lock, wait_lock, unlock)`` over a file descriptor, or None."""
    try:
        import fcntl
    except ImportError:
        fcntl = None
    if fcntl is not None:
        def try_lock(fd):
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False
        return try_lock, lambda fd: fcntl.flock(fd, fcntl.LOCK_EX), lambda fd: fcntl.flock(fd, fcntl.LOCK_UN)
    try:
        import msvcrt
    except ImportError:
        return None

    def windows(fd, mode):
        os.lseek(fd, WINDOWS_LOCK_OFFSET, os.SEEK_SET)
        try:
            msvcrt.locking(fd, mode, 1)
            return True
        except OSError:
            return False

    def wait_lock(fd):
        while not windows(fd, msvcrt.LK_NBLCK):
            time.sleep(0.2)
    return (lambda fd: windows(fd, msvcrt.LK_NBLCK)), wait_lock, lambda fd: windows(fd, msvcrt.LK_UNLCK)


def read_holder(path) -> dict:
    try:
        with open(path, "rb") as handle:
            return json.loads(handle.read(RECORD_SIZE).decode("utf-8").strip() or "{}")
    except (OSError, ValueError):
        return {}


def waiting_line(holder: dict) -> str:
    since = time.strftime("%H:%M:%S", time.localtime(holder["since"])) if "since" in holder else "?"
    who = f"{holder['worktree']} (pid {holder.get('pid', '?')}, since {since})" if holder.get("worktree") \
        else "another worktree"
    return (f"pre-commit: waiting for {who}: lefthook backs up unstaged changes in the stash all "
            "worktrees share. Stage whole files to skip this wait.")


class CommitLock:
    """The common git dir's pre-commit lock; holds the holder's record while locked."""

    def __init__(self, common: str):
        self.path = os.path.join(common, LOCK_NAME)
        self.fd: int | None = None

    def acquire(self, worktree: str, announce=lambda line: print(line, file=sys.stderr, flush=True)) -> bool:
        primitives = _primitives()
        if primitives is None:
            announce("pre-commit: cannot serialize partially staged commits here (neither fcntl nor msvcrt); "
                     "a concurrent partially staged commit in another worktree can swap unstaged changes.")
            return False
        try_lock, wait_lock, _ = primitives
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT | getattr(os, "O_BINARY", 0), 0o666)
        try:
            if not try_lock(fd):
                announce(waiting_line(read_holder(self.path)))
                wait_lock(fd)
        except BaseException:
            os.close(fd)
            raise
        record = json.dumps({"worktree": worktree, "pid": os.getpid(), "since": int(time.time())})
        os.lseek(fd, 0, os.SEEK_SET)
        os.write(fd, record.encode("utf-8").ljust(RECORD_SIZE))
        self.fd = fd
        return True

    def release(self):
        if self.fd is None:
            return
        os.lseek(self.fd, 0, os.SEEK_SET)
        os.write(self.fd, b" " * RECORD_SIZE)
        _primitives()[2](self.fd)
        os.close(self.fd)
        self.fd = None


def _terminal_sent(signum) -> bool:
    """SIGINT/SIGQUIT from our terminal already reached lefthook in our process group."""
    if signum not in (signal.SIGINT, getattr(signal, "SIGQUIT", None)):
        return False
    for stream in (sys.stdin, sys.stderr, sys.stdout):
        try:
            if stream is not None and os.isatty(stream.fileno()):
                return os.tcgetpgrp(stream.fileno()) == os.getpgrp()
        except (OSError, ValueError, AttributeError):
            continue
    return False


def run(command: str, arguments: list[str], environment) -> int:
    child = subprocess.Popen([command, *arguments], env=environment)
    if os.name != "nt":
        def forward(signum, _frame):
            if not _terminal_sent(signum) and child.poll() is None:
                child.send_signal(signum)
        for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"):
            signal.signal(getattr(signal, name), forward)
    while True:
        try:
            return child.wait()
        except KeyboardInterrupt:  # Windows console Ctrl-C reached lefthook too
            continue


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        return self_test()
    if not argv or argv[0].startswith("-"):
        # Hooks pass `run <hook> [ARG ...]`; refuse anything else instead of guessing.
        print("usage: lefthook_serial.py --self-test | COMMAND [ARG ...] (a Git hook's lefthook "
              "command line, run by the real lefthook)", file=sys.stderr)
        return 2
    lefthook = real_lefthook(os.environ)
    if lefthook is None:
        print("lefthook_serial: no lefthook found (LEFTHOOK_BIN, PATH, ~/.local/bin); "
              "install lefthook or set LEFTHOOK_BIN to it", file=sys.stderr)
        return 127
    environment = child_environment(os.environ)
    lock = None
    if argv[:2] == ["run", "pre-commit"]:
        common = common_dir()
        if common and os.environ.get(HELD) != common and partially_staged_paths():
            lock = CommitLock(common)
            try:
                if lock.acquire(os.getcwd()):
                    environment[HELD] = common
            except KeyboardInterrupt:
                return 130
    try:
        status = run(lefthook, argv, environment)
    finally:
        if lock is not None:
            lock.release()
    if status < 0 and os.name != "nt":
        signal.signal(-status, signal.SIG_DFL)
        os.kill(os.getpid(), -status)
    return status if status >= 0 else 128 - status


# ---------------------------------------------------------------- self-test

RC = SELF.parent / "pre-commit" / "lefthook-rc.sh"
BASE = [f"line {number}" for number in range(1, 9)]
RACE_JOB = """case "${RACE_ROLE-}" in
  first) : > "$RACE_SYNC/first"; i=0
    while [ ! -e "$RACE_SYNC/second" ] && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i+1)); done ;;
  second) : > "$RACE_SYNC/second"; sleep 2 ;;
esac
"""


def _isolated(scratch: Path, lefthook: str) -> dict[str, str]:
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("GIT_", "LEFTHOOK")) and key not in (PRESERVED, HELD)}
    config = scratch / "gitconfig"
    config.write_text("[init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n", encoding="utf-8")
    environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=str(config), LEFTHOOK_BIN=lefthook,
                       GIT_AUTHOR_NAME="lefthook serial", GIT_AUTHOR_EMAIL="serial@example.invalid",
                       GIT_COMMITTER_NAME="lefthook serial", GIT_COMMITTER_EMAIL="serial@example.invalid",
                       RACE_SYNC=str(scratch / "sync"), RACE_JOB=str(scratch / "job.sh"))
    (scratch / "sync").mkdir()
    (scratch / "job.sh").write_text(RACE_JOB, encoding="utf-8")
    return environment


def _check(arguments, cwd, environment):
    subprocess.run(arguments, cwd=cwd, env=environment, check=True, capture_output=True,
                   stdin=subprocess.DEVNULL)


def _two_worktrees(scratch: Path, environment, *, wrapped: bool) -> tuple[Path, Path]:
    first = scratch / "a"
    (first / "scripts/ci/pre-commit").mkdir(parents=True)
    shutil.copy2(SELF, first / "scripts/ci/lefthook_serial.py")
    shutil.copy2(RC, first / "scripts/ci/pre-commit/lefthook-rc.sh")
    (first / "lefthook.yml").write_text(
        ("rc: scripts/ci/pre-commit/lefthook-rc.sh\n" if wrapped else "")
        + 'pre-commit:\n  jobs:\n    - name: race\n      run: sh "$RACE_JOB"\n', encoding="utf-8")
    (first / "file.txt").write_text("\n".join(BASE) + "\n", encoding="utf-8")
    _check(["git", "init", "-q", "."], first, environment)
    _check(["git", "add", "-A"], first, environment)
    _check(["git", "commit", "-q", "-m", "init"], first, environment)
    _check([environment["LEFTHOOK_BIN"], "install"], first, environment)
    _check(["git", "worktree", "add", "-q", "-b", "b", str(scratch / "b")], first, environment)
    return first, scratch / "b"


def _stage_partially(tree: Path, tag: str, environment) -> str:
    lines = list(BASE)
    lines[0] = f"{tag} staged"
    (tree / "file.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    _check(["git", "add", "file.txt"], tree, environment)
    lines[5] = f"{tag} unstaged"
    (tree / "file.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return "\n".join(lines) + "\n"


def _commit(tree: Path, environment, role: str = "") -> subprocess.Popen:
    return subprocess.Popen(["git", "commit", "-q", "-m", role or "change"], cwd=tree,
                            env={**environment, "RACE_ROLE": role}, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)


def _until(predicate, seconds: float, process: subprocess.Popen, what: str):
    deadline = time.monotonic() + seconds
    while not predicate():
        if process.poll() is not None:
            raise AssertionError(f"{what}: commit ended first:\n{process.communicate()[0]}")
        if time.monotonic() > deadline:
            raise AssertionError(f"{what}: not within {seconds}s")
        time.sleep(0.05)


def _finish(process: subprocess.Popen, what: str) -> str:
    try:
        return process.communicate(timeout=60)[0]
    except subprocess.TimeoutExpired:
        process.kill()
        raise AssertionError(f"{what} did not finish in 60s: it waited on the lock") from None


def race(lefthook: str, *, wrapped: bool) -> tuple[list[str], str]:
    """Two worktrees commit partially staged file.txt at once; what went wrong?"""
    with tempfile.TemporaryDirectory(prefix="lefthook-serial-") as raw:
        scratch = Path(raw)
        environment = _isolated(scratch, lefthook)
        first, second = _two_worktrees(scratch, environment, wrapped=wrapped)
        expected = {first: _stage_partially(first, "a", environment),
                    second: _stage_partially(second, "b", environment)}
        one = _commit(first, environment, "first")
        _until((scratch / "sync/first").exists, 30, one, "first pre-commit job")
        two = _commit(second, environment, "second")
        outputs = {first: _finish(one, "first commit"), second: _finish(two, "second commit")}
        problems = []
        for (tree, text), process in zip(expected.items(), (one, two)):
            committed = subprocess.run(["git", "show", "HEAD:file.txt"], cwd=tree, env=environment,
                                       capture_output=True, text=True).stdout.splitlines()
            if process.returncode:
                problems.append(f"{tree.name}: commit exited {process.returncode}: {outputs[tree]}")
            elif committed[:1] != [f"{tree.name} staged"] or committed[5:6] != ["line 6"]:
                problems.append(f"{tree.name}: committed {committed}")
            worktree = (tree / "file.txt").read_text(encoding="utf-8")
            if worktree != text:
                problems.append(f"{tree.name}: worktree file is {worktree.splitlines()}, expected {text.splitlines()}")
        return problems, outputs[second]


def lock_held_cases(lefthook: str):
    """Whole-file and nested commits pass a held lock; a partial commit waits for it."""
    with tempfile.TemporaryDirectory(prefix="lefthook-serial-") as raw:
        scratch = Path(raw)
        environment = _isolated(scratch, lefthook)
        first, _ = _two_worktrees(scratch, environment, wrapped=True)
        # Inside a commit hook the inherited GIT_* names the outer repository.
        common = common_dir(first, environment)
        holder = CommitLock(common)
        assert holder.acquire("test-holder", announce=lambda line: None)
        try:
            (first / "file.txt").write_text("whole\n" + "\n".join(BASE[1:]) + "\n", encoding="utf-8")
            _check(["git", "add", "file.txt"], first, environment)
            whole = _commit(first, environment)
            output = _finish(whole, "whole-file commit")
            assert whole.returncode == 0 and "waiting" not in output, f"whole-file commit waited:\n{output}"
            _stage_partially(first, "nested", environment)
            nested = _commit(first, {**environment, HELD: common})
            output = _finish(nested, "nested commit")
            assert nested.returncode == 0 and "waiting" not in output, f"nested commit waited:\n{output}"
            expected = _stage_partially(first, "waits", environment)
            waiting = _commit(first, environment)
            time.sleep(2)
            if waiting.poll() is not None:
                raise AssertionError("a partially staged commit ran while another worktree held the lock:\n"
                                     + waiting.communicate()[0])
        finally:
            holder.release()
        output = _finish(waiting, "partially staged commit after release")
        assert waiting.returncode == 0, output
        assert output.count("pre-commit: waiting for test-holder (pid ") == 1, output
        assert (first / "file.txt").read_text(encoding="utf-8") == expected


def unit_tests():
    # P1-P4 partial staging is the intersection of both NUL-separated listings.
    assert partially_staged(b"", b"") == []
    assert partially_staged(b"a.txt\0b c.txt\0", b"d.txt\0") == []
    assert partially_staged(b"a.txt\0b c.txt\0", b"b c.txt\0a.txt\0e\0") == ["a.txt", "b c.txt"]
    assert partially_staged(b"new\nline\0", b"new\nline\0") == ["new\nline"]
    with tempfile.TemporaryDirectory(prefix="lefthook-serial-") as raw:
        root = Path(raw)
        environment = _isolated(root, "lefthook")
        _check(["git", "init", "-q", "."], root, environment)
        (root / "f").write_text("1\n", encoding="utf-8")
        _check(["git", "add", "f"], root, environment)
        _check(["git", "commit", "-q", "-m", "i"], root, environment)
        detect = lambda: subprocess.run([sys.executable, "-c", "import sys; sys.path.insert(0, sys.argv[1]);"
                                         "import lefthook_serial as s; print(s.partially_staged_paths())",
                                         str(SELF.parent)], cwd=root, env=environment, check=True,
                                        capture_output=True, text=True).stdout.strip()
        (root / "f").write_text("2\n", encoding="utf-8")
        assert detect() == "[]", "P5 unstaged only"
        _check(["git", "add", "f"], root, environment)
        assert detect() == "[]", "P6 whole file staged"
        (root / "f").write_text("3\n", encoding="utf-8")
        (root / "g").write_text("new\n", encoding="utf-8")
        _check(["git", "add", "g"], root, environment)
        assert detect() == "['f']", "P7 staged and unstaged"
        fake = root / "bin/lefthook"
        fake.parent.mkdir()
        fake.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        fake.chmod(0o755)
        alias = root / "alias/lefthook"
        alias.parent.mkdir()
        try:
            alias.symlink_to(SELF)
        except OSError:
            alias = None
        # L1-L3 the wrapper never resolves to itself, by name, path or symlink.
        path = os.pathsep.join(str(item) for item in (alias.parent if alias else root, fake.parent))
        assert real_lefthook({PRESERVED: str(SELF), "PATH": path}) == str(fake)
        assert real_lefthook({PRESERVED: str(fake), "PATH": ""}) == str(fake)
        assert real_lefthook({PRESERVED: "", "PATH": str(root / "missing")}) in (
            None, shutil.which(str(Path.home() / ".local/bin/lefthook")))
    # E1-E3 the caller's LEFTHOOK_BIN comes back; the rc's variable never leaks.
    assert child_environment({PRESERVED: "/x/lefthook", "LEFTHOOK_BIN": str(SELF)}) == {"LEFTHOOK_BIN": "/x/lefthook"}
    assert child_environment({PRESERVED: "", "LEFTHOOK_BIN": str(SELF), "A": "1"}) == {"A": "1"}
    assert child_environment({"LEFTHOOK_BIN": "/x/lefthook"}) == {"LEFTHOOK_BIN": "/x/lefthook"}
    assert waiting_line({}).startswith("pre-commit: waiting for another worktree: ")


def rc_tests():
    shell = shutil.which("sh")
    if shell is None:
        print("lefthook_serial self-test: rc checks skipped: no sh on PATH")
        return
    root = SELF.parents[2]
    probe = '. scripts/ci/pre-commit/lefthook-rc.sh; printf "%s|%s" "$LEFTHOOK_BIN" "${AWAKEN_LEFTHOOK_BIN-unset}"'
    base = {key: value for key, value in os.environ.items() if key not in ("LEFTHOOK_BIN", PRESERVED)}
    run_rc = lambda extra: subprocess.run([shell, "-c", probe], cwd=root, env={**base, **extra},
                                          capture_output=True, text=True, check=True).stdout
    wrapper = "/elsewhere/scripts/ci/lefthook_serial.py"
    # R1 a caller's LEFTHOOK_BIN is preserved; R2 unset stays empty; R3 a leaked wrapper is not.
    assert run_rc({"LEFTHOOK_BIN": "/opt/lefthook"}).endswith("/scripts/ci/lefthook_serial.py|/opt/lefthook")
    assert run_rc({}).endswith("/scripts/ci/lefthook_serial.py|"), run_rc({})
    assert run_rc({"LEFTHOOK_BIN": wrapper}).endswith("|unset")


def status_tests():
    if os.name == "nt":
        print("lefthook_serial self-test: signal checks skipped on Windows")
        return
    with tempfile.TemporaryDirectory(prefix="lefthook-serial-") as raw:
        root = Path(raw)
        fake = root / "lefthook"
        fake.write_text('#!/bin/sh\ncase "$1" in\n  seven) exit 7 ;;\n  die) kill -TERM $$ ;;\n'
                        '  wait) trap "exit 42" TERM; : > "$2"; while :; do sleep 0.05; done ;;\nesac\n',
                        encoding="utf-8")
        fake.chmod(0o755)
        environment = {**os.environ, PRESERVED: str(fake)}
        wrapper = [sys.executable, str(SELF)]
        # S0 a flag no hook passes is refused; S1 exit status unchanged; S2 lefthook's own
        # signal death is ours; S3 TERM is forwarded.
        assert subprocess.run([*wrapper, "--verbose"], cwd=root, env=environment,
                              capture_output=True).returncode == 2
        assert subprocess.run([*wrapper, "seven"], cwd=root, env=environment).returncode == 7
        assert subprocess.run([*wrapper, "die"], cwd=root, env=environment).returncode == -signal.SIGTERM
        ready = root / "ready"
        waiting = subprocess.Popen([*wrapper, "wait", str(ready)], cwd=root, env=environment)
        _until(ready.exists, 30, waiting, "fake lefthook start")
        waiting.send_signal(signal.SIGTERM)
        assert waiting.wait(timeout=30) == 42


def self_test() -> int:
    unit_tests()
    rc_tests()
    status_tests()
    lefthook = real_lefthook({**os.environ, PRESERVED: ""})
    if lefthook is None:
        print("lefthook_serial self-test: race checks SKIPPED: no lefthook on PATH or ~/.local/bin")
        return 0
    started = time.monotonic()
    unwrapped, _ = race(lefthook, wrapped=False)
    assert unwrapped, "the unwrapped race no longer corrupts: re-check lefthook's backup and this wrapper"
    print("lefthook_serial self-test: without the wrapper: " + "; ".join(unwrapped))
    wrapped, output = race(lefthook, wrapped=True)
    assert not wrapped, "with the wrapper: " + "; ".join(wrapped)
    assert "pre-commit: waiting for " in output, output
    lock_held_cases(lefthook)
    print(f"lefthook_serial self-test: unwrapped race corrupted, wrapped race kept both worktrees, "
          f"whole-file and nested commits did not wait ({time.monotonic() - started:.1f}s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
