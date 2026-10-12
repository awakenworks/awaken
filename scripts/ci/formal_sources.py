"""Which bytes the formal gates read: the index in a repository, the disk otherwise.

The obligation ledger is assembled from TOML files and from anchors written in
Rust sources. Both are read here, and only here, so every gate judges the same
bytes. In a Git work tree that is the index (``git grep --cached``): the staged
truth a pre-commit hook judges and a clean CI checkout holds, so an edit is seen
once it is staged. Outside a repository -- a checker's fixture tree -- the files
on disk are read.

Consumers: ``formal_ledger`` (TOML storage), ``formal_anchors`` (Rust anchors)
and the fixtures of their self-tests. The working-plan and formal/release
views and the source guards also share Git environment selection and raw source facts here; their
different receipt shapes and qualification rules remain with those consumers.
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
from collections.abc import Callable, Iterable
from pathlib import Path

# Git exports these to a hook. They name the repository being committed, not
# whichever directory a checker happens to read -- a fixture tree under /tmp
# would otherwise be judged against the hook's repository and index.
_REPOSITORY_ENVIRONMENT = (
    "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX", "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
)


class SourceError(ValueError):
    """The index cannot be read."""


def detached_git_environment() -> dict[str, str]:
    """The environment with every repository-selecting Git variable removed.

    Consumers: repository discovery below, and fixtures that run Git in a
    temporary tree while a hook's variables are exported.
    """

    return {key: value for key, value in os.environ.items() if key not in _REPOSITORY_ENVIRONMENT}


def repository_environment(root: Path) -> dict[str, str] | None:
    """How to read ``root``'s index, or None when ``root`` is not a work-tree top.

    The repository is discovered from ``root`` alone. When a hook exported
    GIT_DIR for that same repository, its GIT_INDEX_FILE is the index being
    committed and is honoured; a hook's variables for any other repository are
    dropped.
    """

    clean = detached_git_environment()
    try:
        found = subprocess.run(
            ["git", "rev-parse", "--show-toplevel", "--absolute-git-dir"], cwd=root, text=True,
            capture_output=True, check=False, env=clean,
        )
    except OSError:
        return None
    lines = found.stdout.splitlines()
    if found.returncode != 0 or len(lines) != 2 or Path(lines[0]).resolve() != root.resolve():
        return None
    exported = os.environ.get("GIT_DIR")
    work_tree = os.environ.get("GIT_WORK_TREE")
    matching_tree = work_tree is None or (root / work_tree).resolve() == root.resolve()
    if exported is not None and (root / exported).resolve() == Path(lines[1]).resolve() and matching_tree:
        return dict(os.environ)
    return clean


def _source_environment(root: Path) -> dict[str, str]:
    values = repository_environment(root)
    if values is None:
        # Preserve Git's real 128/OSError for an absent repository/tool. If Git
        # succeeds through a parent repository, this root is still invalid.
        subprocess.check_output(["git", "-C", str(root), "rev-parse", "--show-toplevel"],
                                env=detached_git_environment())
        raise SourceError(f"Source root is not an admitted repository top: {root}")
    return values


def _git_output(root: Path, environment: dict[str, str], *arguments: str) -> bytes:
    return subprocess.check_output(
        ["git", "-c", "core.fsmonitor=false", "-C", str(root), *arguments], env=environment,
    )


def git_output(root: Path, *arguments: str) -> bytes:
    """Git bytes for an exact root, preserving only its own hook's index."""
    return _git_output(root, _source_environment(root), *arguments)


def _path_names(output: bytes) -> list[str]:
    """Names from an explicitly NUL-framed Git command, never display lines."""
    return [os.fsdecode(name) for name in output.split(b"\0") if name]


def git_paths(root: Path, *arguments: str) -> list[str]:
    """Exact path identities from a caller's Git command carrying ``-z``."""
    return _path_names(git_output(root, *arguments))


def tracked_files(root: Path, *, allow_nonrepository: bool = False) -> list[str]:
    """Tracked names, with an explicitly admitted non-repository fixture walk.

    Discovery alone may admit the disk fixture. Once a repository is found,
    an empty list stays empty and a failed index read stays a Git failure.
    Missing Git remains an OSError, rather than admitting a disk substitute.
    """
    try:
        environment = _source_environment(root)
    except (subprocess.CalledProcessError, SourceError):
        if not allow_nonrepository or (root / ".git").exists() or (root / ".git").is_symlink():
            raise
        return [path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()]
    return _path_names(_git_output(root, environment, "ls-files", "-z"))


def file_bytes(root: Path, relative: str, staged: bool) -> bytes | None:
    """A selected index blob or working file; only missing disk files are None."""
    if staged:
        return git_output(root, "show", f":{relative}")
    path = root / relative
    return path.read_bytes() if path.is_file() else None


def file_text(root: Path, relative: str, staged: bool) -> str | None:
    data = file_bytes(root, relative, staged)
    if data is None:
        return None
    text = data.decode("utf-8", errors="replace")
    # Path.read_text used universal newlines for working files. Index blobs
    # already used a raw decode; preserve those two existing contracts.
    return text if staged else text.replace("\r\n", "\n").replace("\r", "\n")


def committed_text(root: Path, relative: str) -> str | None:
    """HEAD's file, distinguishing an unborn/absent path from failed Git IO."""
    try:
        git_output(root, "rev-parse", "--verify", "--quiet", "HEAD")
    except subprocess.CalledProcessError as error:
        if error.returncode != 1:
            raise
        # Git's quiet lookup also returns1 for malformed loose refs. Git's
        # symbolic-ref refuses those; verify the branch is absent through Git's
        # own ref storage, rather than treating a missing loose file as unborn.
        branch = git_output(root, "symbolic-ref", "--quiet", "HEAD").decode().strip()
        try:
            git_output(root, "show-ref", "--verify", "--quiet", branch)
        except subprocess.CalledProcessError as absent:
            if absent.returncode == 1:
                return None
            raise
        raise SourceError(f"HEAD's existing branch cannot be resolved: {branch}") from error
    if not git_paths(root, "ls-tree", "-z", "--name-only", "--full-tree", "HEAD", "--",
                     f":(literal){relative}"):
        return None
    return git_output(root, "show", f"HEAD:{relative}").decode("utf-8", errors="replace")


def allow_reason(line: str, rule: str) -> str | None:
    """The shared marker spelling, without deciding scope or permission.

    A blank payload is distinct from no marker: the secret owner admits it,
    while the wait owner requires 20 characters and the ceiling owner a number.
    Metadata stays opaque text; this reader adds no owner/expiry/reason policy.
    """
    match = re.search(r"awaken-allow\(" + re.escape(rule) + r"\):(?P<reason>.*)$", line)
    return match.group("reason").strip() if match is not None else None


def framed_digest(parts: Iterable[bytes]) -> str:
    """The existing working digest's length framing, shared by dirty diagnostics."""
    digest = hashlib.sha256()
    for value in parts:
        digest.update(len(value).to_bytes(8, "big"))
        digest.update(value)
    return digest.hexdigest()


def _untracked_sources(root: Path, read: Callable[..., bytes]) -> dict[str, bytes]:
    paths = _path_names(read("ls-files", "--others", "--exclude-standard", "-z"))
    return {name: (root / name).read_bytes() for name in paths}


def untracked_sources(root: Path) -> dict[str, bytes]:
    return _untracked_sources(root, lambda *arguments: git_output(root, *arguments))


def untracked_parts(files: dict[str, bytes]) -> Iterable[bytes]:
    for name, data in sorted(files.items()):
        yield os.fsencode(name)
        yield data


def _source_facts(root: Path, read: Callable[..., bytes]) -> dict:
    revision = read("rev-parse", "HEAD")
    revision_name = revision.decode().strip()
    return {
        "revision": revision,
        "tree": read("rev-parse", revision_name + "^{tree}"),
        # Entries bind mode/object/stage/path even when the index is unmerged.
        # Formal dirty diagnostics cannot require write-tree to succeed.
        "index_entries": read("ls-files", "--stage", "-z"),
        "staged": read("diff", "--cached", "--binary", revision_name, "--"),
        "working": read("diff", "--binary", "--"),
        "patch": read("diff", "--binary", revision_name, "--"),
        "untracked": _untracked_sources(root, read),
        "status": read("status", "--porcelain=v1", "--untracked-files=all"),
    }


def source_facts(root: Path, *, with_index_tree: bool = False) -> dict:
    """Shared raw facts, not a receipt/schema or an atomic/ABA-proof snapshot.

    Repeat the same reader to refuse a changed axis during capture. Working
    consumers can bind write-tree between these two reads; formal diagnostics
    retain their original shape and can still describe an unmerged index.
    The consumer views own working identity versus clean-release policy.
    """
    environment = _source_environment(root)
    def read(*arguments: str) -> bytes:
        return _git_output(root, environment, *arguments)
    facts = _source_facts(root, read)
    index = read("write-tree") if with_index_tree else None
    if facts != _source_facts(root, read):
        raise SourceError("Source changed while capturing Git source facts")
    if with_index_tree:
        facts["index_tree"] = index
    return facts


def _git_grep(root: Path, environment: dict[str, str], arguments: list[str]) -> bytes:
    # Explicit flags, so a user's grep.* configuration cannot change the record
    # shape or the pattern language.
    result = subprocess.run(
        ["git", "grep", "--cached", "--no-color", "--full-name", "-I", *arguments],
        cwd=root, capture_output=True, check=False, env=environment,
    )
    if result.returncode not in (0, 1):
        raise SourceError(
            f"git grep --cached {' '.join(arguments)} failed: "
            f"{result.stderr.decode(errors='replace').strip()}"
        )
    return result.stdout


def _staged_text(
    root: Path, pattern: str, containing: tuple[str, ...], environment: dict[str, str]
) -> dict[str, str]:
    pathspecs = [f":(glob){pattern}"]
    if containing:
        needles = [argument for needle in containing for argument in ("-e", needle)]
        listed = _git_grep(root, environment, ["-l", "-z", "--fixed-strings", *needles, "--", *pathspecs])
        pathspecs = [f":(literal){path}" for path in listed.decode("utf-8").split("\0") if path]
        if not pathspecs:
            return {}
    # Every line of every selected staged blob, as `path NUL line NUL text`.
    output = _git_grep(root, environment, ["-z", "-n", "--basic-regexp", "-e", "^", "--", *pathspecs])
    lines: dict[str, list[str]] = {}
    for record in output.decode("utf-8").split("\n")[:-1]:
        path, _, rest = record.partition("\0")
        _, _, text = rest.partition("\0")
        lines.setdefault(path, []).append(text + "\n")
    return {path: "".join(body) for path, body in sorted(lines.items())}


def _worktree_text(root: Path, pattern: str, containing: tuple[str, ...]) -> dict[str, str]:
    found: dict[str, str] = {}
    for path in sorted(root.glob(pattern)):
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if not containing or any(needle in text for needle in containing):
            found[path.relative_to(root).as_posix()] = text
    return found


def source_text(root: Path, pattern: str, containing: tuple[str, ...] = ()) -> dict[str, str]:
    """Path -> text of every file under ``pattern`` (a pathlib-style glob).

    With ``containing``, only files holding at least one of those literal
    strings. Staged bytes in a repository, bytes on disk otherwise.
    """

    environment = repository_environment(root)
    if environment is None:
        return _worktree_text(root, pattern, containing)
    return _staged_text(root, pattern, containing, environment)


def matching_lines(root: Path, needle: str, accept: Callable[[str], object]) -> list[tuple[str, int, str]]:
    """(path, 1-based line, text) of every line holding ``needle`` in an accepted file.

    ``accept`` decides by repository-relative path, so the caller's own rule is
    the scope and no path list is restated here. One ``git grep --cached`` over
    the index in a repository; a walk of the tree on disk otherwise (following
    links, as a fixture tree links the repository it copies from). Consumer:
    the file-head anchor scan, which needs only anchor lines and their numbers.
    """

    environment = repository_environment(root)
    found: list[tuple[str, int, str]] = []
    if environment is None:
        for directory, directories, names in os.walk(root, followlinks=True):
            directories[:] = sorted(name for name in directories if not name.startswith("."))
            for name in sorted(names):
                path = Path(directory) / name
                relative = path.relative_to(root).as_posix()
                if not accept(relative):
                    continue
                try:
                    text = path.read_text(encoding="utf-8")
                except (UnicodeDecodeError, OSError):
                    continue
                found += [
                    (relative, number, line)
                    for number, line in enumerate(text.split("\n"), 1)
                    if needle in line
                ]
        return sorted(found)
    output = _git_grep(root, environment, ["-z", "-n", "--fixed-strings", "-e", needle])
    for record in output.decode("utf-8").split("\n")[:-1]:
        path, _, rest = record.partition("\0")
        number, _, text = rest.partition("\0")
        if accept(path):
            found.append((path, int(number), text))
    return sorted(found)


def _selftest_guard_inputs() -> None:
    """The guard input boundary's cause/effect cases, using actual Git and mains.

    Valid root + same-hook index -> selected index bytes; foreign hook ->
    root's own bytes; failed index/blob/tool -> original exception, never an
    empty success. Discovery may admit an explicit non-Git fixture only.
    SIO1 own alternate index / default working bytes; SIO2 foreign hook;
    SIO3 bad index/blob/HEAD/tool; SIO4 empty/nonrepo/unborn/absent HEAD path;
    SIO5 exact NUL path identity and newline decoding; SIO6 one hygiene read
    feeds both original text and executable matchers. All fixture mutations
    are detached from the invoking hook; its repository is never initialized.
    """
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    import check_file_limits
    import check_repository_hygiene
    import check_secrets

    environment = detached_git_environment()
    owners = (check_secrets, check_file_limits, check_repository_hygiene)

    def git(root, *arguments):
        return subprocess.check_output(["git", *arguments], cwd=root, env=environment)

    def prepare(root):
        root.mkdir()
        (root / "docs").mkdir()
        (root / "probe.py").write_text("print(1)\n")
        (root / "docs/probe.md").write_text("# short\n")
        for arguments in (("init", "--quiet"), ("add", "."),
                          ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                           "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
            git(root, *arguments)
        git(root, "config", "core.autocrlf", "false")

    def invoke(owner, root, arguments, hook):
        output, errors = io.StringIO(), io.StringIO()
        with patch.object(owner, "REPO_ROOT", root), patch.dict(os.environ, hook, clear=True), \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            return owner.main(arguments), output.getvalue(), errors.getvalue()

    with tempfile.TemporaryDirectory(prefix="awaken-guard-inputs-") as temporary:
        directory = Path(temporary)
        root, foreign = directory / "repo", directory / "foreign"
        prepare(root)
        prepare(foreign)
        (root / "probe.py").write_text('token = "' + 'abc12345' + 'SECRET"\n# bad\u200b\n')
        (root / "docs/probe.md").write_text("# large\n" * 1201)
        git(root, "add", ".")
        partial = root / ".git/partial-index"
        subprocess.check_output(["git", "read-tree", "HEAD"], cwd=root,
                                env={**environment, "GIT_INDEX_FILE": str(partial)})
        own = {**environment, "GIT_DIR": str(root / ".git"), "GIT_WORK_TREE": str(root),
               "GIT_INDEX_FILE": str(partial)}
        decoy = {**environment, "GIT_DIR": str(foreign / ".git"), "GIT_WORK_TREE": str(foreign),
                 "GIT_INDEX_FILE": str(foreign / ".git/index"), "GIT_PREFIX": "hook/"}
        protected = {path.relative_to(foreign).as_posix(): path.read_bytes()
                     for path in foreign.rglob("*") if path.is_file()}
        for owner in owners:
            # SIO1: a real own alternate index is clean while working bytes
            # are bad. Staged succeeds; default still scans the working file.
            assert invoke(owner, root, ["--staged"], own)[0] == 0, "SIO1 own partial index"
            assert invoke(owner, root, [], own)[0] == 1, "SIO1 default working bytes"
            # SIO2: the same actual main cannot substitute the foreign clean
            # index. The old copies returned0 without scanning the bad source.
            assert invoke(owner, root, ["--staged"], decoy)[0] == 1, "SIO2 foreign hook cannot hide debt"
        assert protected == {path.relative_to(foreign).as_posix(): path.read_bytes()
                             for path in foreign.rglob("*") if path.is_file()}, "SIO2 no foreign mutation"

        broken = root / ".git/broken-index"
        broken.write_bytes(b"not an index")
        for owner in owners:
            # SIO3: an actual corrupted index returns Git128; no guard may
            # translate it into its successful no-staged-files message.
            try:
                invoke(owner, root, ["--staged"], {**own, "GIT_INDEX_FILE": str(broken)})
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 preserve Git failure"
            else:
                raise AssertionError("SIO3 broken index became an empty PASS")
            # Invalid argv must be rejected before that same failing read.
            try:
                invoke(owner, root, ["--unknown"], {**own, "GIT_INDEX_FILE": str(broken)})
            except SystemExit as error:
                assert error.code == 2, "SIO3 parse before IO"
            else:
                raise AssertionError("SIO3 unknown argv admitted")

        # SIO5: spaces/Unicode/option-shaped names are portable. Raw undecodable
        # bytes and newline filenames are POSIX filesystem causes, not Windows
        # filenames. Keep every other input/error case active on both systems.
        names = ("space name.py", "nonascii-名.py", "-option.py")
        if os.name != "nt":
            names += ("line\nname.py", os.fsdecode(b"raw-\xff.py"))
        for name in names:
            (root / name).write_bytes(b"print(1)\r\nprint(2)\r")
        git(root, "add", "--", *names)
        with patch.dict(os.environ, environment, clear=True):
            listed = git_paths(root, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR")
            assert set(names) <= set(listed), "SIO5 exact path identity"
            for name in names:
                assert file_text(root, name, True) == "print(1)\r\nprint(2)\r", "SIO5 staged raw decode"
                assert file_text(root, name, False) == "print(1)\nprint(2)\n", "SIO5 working newline decode"
            untracked = root / (os.fsdecode(b"untracked-\xff.py") if os.name != "nt" else "untracked-名.py")
            untracked.write_bytes(b"untracked\n")
            assert untracked_sources(root) == {untracked.name: b"untracked\n"}, "SIO5 one framing primitive preserves source-facts raw filename/bytes"
            assert committed_text(root, "absent.py") is None, "SIO4 genuinely absent HEAD path"
            try:
                file_bytes(root, "absent.py", True)
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 missing selected index blob is an error"
            else:
                raise AssertionError("SIO3 index blob failure became None")

        reads = []
        original = file_bytes
        def observed(actual_root, relative, staged):
            reads.append(relative)
            return original(actual_root, relative, staged)
        with patch.dict(globals(), file_bytes=observed):
            assert invoke(check_repository_hygiene, root, ["--staged"], environment)[0] == 1
        assert len(reads) == len(set(reads)) and "probe.py" in reads, "SIO6 one blob feeds both matchers"
        # SIO5/SIO6: read_text's universal-newline projection and raw bytes'
        # splitlines give identical line scopes to the actual Unicode, license
        # and anchored conflict matchers, including bare CR and CRLF.
        original_text = ("ok\nbad\u200b\n<<<<<<< HEAD\nPermission is hereby granted,"
                         + " free of charge\n")
        for newline in ("\n", "\r\n", "\r"):
            data = original_text.replace("\n", newline).encode()
            raw = check_repository_hygiene.check_text_content({"newline.py": data})
            universal = check_repository_hygiene.check_text_content({"newline.py": original_text.encode()})
            assert raw == universal and len(raw) == 3, "SIO5/SIO6 actual newline matcher scope"

        disk, empty = directory / "disk", directory / "empty"
        disk.mkdir()
        (disk / "fixture.py").write_text("fixture\n")
        empty.mkdir()
        git(empty, "init", "--quiet")
        (empty / "untracked.py").write_text("untracked\n")
        with patch.dict(os.environ, environment, clear=True):
            assert tracked_files(disk, allow_nonrepository=True) == ["fixture.py"], "SIO4 admitted disk fixture"
            assert tracked_files(empty, allow_nonrepository=True) == [], "SIO4 empty Git set is not disk fallback"
            assert committed_text(empty, "untracked.py") is None, "SIO4 unborn HEAD"
        (empty / "tests").mkdir()
        (empty / "tests/new.rs").write_text("fn test() {}\n")
        git(empty, "add", "tests/new.rs")
        assert invoke(check_file_limits, empty, ["--staged"], environment)[0] == 0, "SIO4 unborn short new source"
        with patch.dict(os.environ, {**environment, "PATH": str(disk)}, clear=True):
            try:
                tracked_files(root, allow_nonrepository=True)
            except OSError:
                pass
            else:
                raise AssertionError("SIO3 missing Git became a disk fallback")

        # SIO3/SIO4: a dangling .git link is an invalid repository marker,
        # never the explicitly admitted non-repository fixture case.
        # SIO3: the POSIX physical dangling-link case needs no privilege. NT
        # must not acquire symlink privileges for a Linux source regression;
        # malformed/index/foreign/HEAD cases above still run there. Windows
        # physical execution remains its own qualification, not this Linux run.
        if os.name != "nt":
            (disk / ".git").symlink_to(directory / "absent-git-directory", target_is_directory=True)
            with patch.dict(os.environ, environment, clear=True):
                try:
                    tracked_files(disk, allow_nonrepository=True)
                except subprocess.CalledProcessError as error:
                    assert error.returncode == 128, "SIO3 dangling repository marker"
                else:
                    raise AssertionError("SIO3 dangling .git became a disk fallback")

        branch = git(root, "symbolic-ref", "HEAD").decode().strip()
        revision = git(root, "rev-parse", "HEAD").decode().strip()
        # SIO4: detached and packed valid HEADs still bind the same previous
        # bytes. Absence is established by Git, not by a loose-ref path check.
        git(root, "checkout", "--detach", "--quiet", revision)
        with patch.dict(os.environ, environment, clear=True):
            assert committed_text(root, "probe.py") == "print(1)\n", "SIO4 valid detached HEAD"
            assert committed_text(root, "absent.py") is None, "SIO4 absent path under detached HEAD"
        git(root, "symbolic-ref", "HEAD", branch)
        git(root, "pack-refs", "--all", "--prune")
        with patch.dict(os.environ, environment, clear=True):
            assert committed_text(root, "probe.py") == "print(1)\n", "SIO4 valid packed branch HEAD"
        record = root / ".git" / branch
        record.parent.mkdir(parents=True, exist_ok=True)
        # SIO3/SIO4: quiet rev-parse reports1 both for unborn and malformed
        # loose refs. Existing malformed metadata is a hard source error;
        # the earlier genuine-unborn case must still return None.
        record.write_text("not-an-oid\n")
        with patch.dict(os.environ, environment, clear=True):
            try:
                committed_text(root, "probe.py")
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 malformed existing HEAD"
            else:
                raise AssertionError("SIO3 malformed existing HEAD became unborn")
        record.write_text("a" * 40 + "\n")
        with patch.dict(os.environ, environment, clear=True):
            try:
                committed_text(root, "probe.py")
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 failed HEAD object"
            else:
                raise AssertionError("SIO3 unreadable HEAD became an absent historical file")
        record.unlink()
        (root / ".git/packed-refs").write_text("not-a-packed-ref\n")
        with patch.dict(os.environ, environment, clear=True):
            try:
                committed_text(root, "probe.py")
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 unreadable packed ref storage"
            else:
                raise AssertionError("SIO3 unknown packed storage became unborn")
