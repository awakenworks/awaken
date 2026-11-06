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
import shutil
import subprocess
from collections.abc import Callable, Iterable
from concurrent.futures import ThreadPoolExecutor
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


def _git_command(environment: dict[str, str], *arguments: str) -> list[str]:
    # CreateProcess does not use the supplied child PATH for bare-name lookup.
    # Resolve the caller-selected native Git without replacing an external shim.
    executable = "git"
    if os.name == "nt":
        executable = shutil.which("git.exe", path=environment.get("PATH", os.defpath))
        if executable is None:
            raise FileNotFoundError(2, "Git executable not found in the admitted PATH", "git.exe")
        executable = os.path.abspath(executable)
    return [executable, *arguments]


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
            _git_command(clean, "rev-parse", "--show-toplevel", "--absolute-git-dir"), cwd=root, text=True,
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
        clean = detached_git_environment()
        subprocess.check_output(_git_command(clean, "-C", str(root), "rev-parse", "--show-toplevel"),
                                env=clean)
        raise SourceError(f"Source root is not an admitted repository top: {root}")
    return values


def _git_output(root: Path, environment: dict[str, str], *arguments: str) -> bytes:
    return subprocess.check_output(
        _git_command(environment, "-c", "core.fsmonitor=false", "-c", "diff.autoRefreshIndex=false",
                     "-C", str(root), *arguments), env=environment,
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


def untracked_sources(root: Path) -> dict[str, bytes]:
    paths = git_paths(root, "ls-files", "--others", "--exclude-standard", "-z")
    return {name: (root / name).read_bytes() for name in paths}


def _index_and_untracked(root: Path, read: Callable[..., bytes]) -> tuple[bytes, dict[str, bytes]]:
    # Git tags each NUL-terminated row before the pathname (or stage header).
    # Removing the tags gives the same ordered stage records as --stage alone.
    records = read("ls-files", "-t", "--stage", "--others", "--exclude-standard", "-z").split(b"\0")
    if records.pop() != b"":
        raise SourceError("Unterminated Git file record")
    entries = bytearray()
    untracked = {}
    for row in records:
        tag, payload = row[:2], row[2:]
        if tag == b"? " and payload:
            name = os.fsdecode(payload)
            untracked[name] = (root / name).read_bytes()
        elif tag in (b"H ", b"M ", b"S ") and b"\t" in payload:
            entries.extend(payload)
            entries.append(0)
        else:
            raise SourceError("Malformed Git file record")
    return bytes(entries), untracked


def untracked_parts(files: dict[str, bytes]) -> Iterable[bytes]:
    for name, data in sorted(files.items()):
        yield os.fsencode(name)
        yield data


def _source_facts(root: Path, read: Callable[..., bytes]) -> dict:
    # Read both object IDs from one commit object. Force the output shape even
    # in repositories with configured notes, signatures or decorations.
    ids = read("show", "-s", "--format=%H%n%T", "--no-show-signature", "--no-notes",
               "--no-color", "--no-decorate", "HEAD", "--").splitlines(keepends=True)
    if len(ids) != 2 or not all(re.fullmatch(rb"(?:[a-f0-9]{40}|[a-f0-9]{64})\n", oid) for oid in ids):
        raise SourceError("Malformed Git commit identity")
    revision, tree = ids
    if len(revision) != len(tree):
        raise SourceError("Inconsistent Git commit and tree object formats")
    revision_name = revision.decode("ascii").rstrip("\n")
    reads = {
        "staged": lambda: read("diff", "--cached", "--binary", revision_name, "--"),
        "working": lambda: read("diff", "--binary", "--"),
        "patch": lambda: read("diff", "--binary", revision_name, "--"),
    }
    # Bound concurrency within each frame; keep both complete frames and
    # the separate read-only index-tree validation between them.
    with ThreadPoolExecutor(max_workers=4) as executor:
        files = executor.submit(_index_and_untracked, root, read)
        values = executor.map(lambda operation: operation(), reads.values())
        index_entries, untracked = files.result()
        facts = {"revision": revision, "tree": tree, "index_entries": index_entries,
                 **dict(zip(reads, values)), "untracked": untracked}
    # Keep the original end-of-frame boundary after every earlier read joins.
    facts["status"] = read("status", "--porcelain=v1", "--untracked-files=all")
    return facts


def source_facts(root: Path, *, with_index_tree: bool = False) -> dict:
    """Shared raw facts, not a receipt/schema or an atomic/ABA-proof snapshot.

    Repeat the same reader to refuse a changed axis during capture. Working
    consumers bind a read-only index tree between these two reads; formal diagnostics
    retain their original shape and can still describe an unmerged index.
    The consumer views own working identity versus clean-release policy.
    """
    environment = _source_environment(root)
    # Status/diff may refresh stat caches unless optional locks are disabled.
    # Capturing evidence must leave even an unlocked caller's index untouched.
    environment["GIT_OPTIONAL_LOCKS"] = "0"
    def read(*arguments: str) -> bytes:
        return _git_output(root, environment, *arguments)
    facts = _source_facts(root, read)
    index = None
    if with_index_tree:
        revision = facts["revision"].decode().strip()
        algorithm = "sha256" if len(facts["tree"].strip()) == 64 else "sha1"
        entries = facts["index_entries"]
        empty_blob = _object_id(algorithm, b"blob", b"").hex().encode("ascii")
        existing: set[bytes] = set()
        staged: set[bytes] = set()
        # Only the empty blob can represent an intent-to-add entry. Avoid the
        # extra Git reads when no index entry needs that disambiguation.
        if b" " + empty_blob + b" 0\t" in entries:
            existing = set(read("ls-tree", "-r", "--name-only", "-z", revision).split(b"\0"))
            staged = set(read("diff", "--cached", "--name-only", "-z", revision, "--").split(b"\0"))
        index = _index_tree(entries, existing, staged, algorithm)
    if facts != _source_facts(root, read):
        raise SourceError("Source changed while capturing Git source facts")
    if with_index_tree:
        facts["index_tree"] = index
    return facts


def _object_id(algorithm: str, kind: bytes, body: bytes) -> bytes:
    framed = kind + b" " + str(len(body)).encode("ascii") + b"\0" + body
    return hashlib.new(algorithm, framed, usedforsecurity=False).digest()


def _index_tree(entries: bytes, existing: set[bytes], staged: set[bytes], algorithm: str) -> bytes:
    """Git's index tree identity, without an index lock or an object write.

    Working capture has already selected one exact Git root and its own hook
    index. Its NUL-framed entries carry mode/object/stage/path. HEAD and staged
    names distinguish a genuinely staged empty file from an intent-to-add,
    which Git excludes from write-tree. Both extra reads remain inside the
    existing before/after source fence; formal diagnostics need no tree.
    """
    empty_oid = _object_id(algorithm, b"blob", b"")
    empty_blob = empty_oid.hex().encode("ascii")
    tree: dict = {}
    for record in entries.split(b"\0"):
        if not record:
            continue
        try:
            header, path = record.split(b"\t", 1)
            mode, blob, stage = header.split()
            oid = bytes.fromhex(blob.decode("ascii"))
            numeric_mode = int(mode, 8)
        except (ValueError, UnicodeError) as error:
            raise SourceError("Malformed Git index entry") from error
        if stage != b"0":
            raise SourceError("An unmerged index cannot qualify execution")
        if len(oid) != len(empty_oid) or numeric_mode not in {
            0o100644, 0o100755, 0o120000, 0o160000, 0o40000,
        }:
            raise SourceError("Invalid Git index object or mode")
        if path not in existing and path not in staged and blob == empty_blob:
            continue
        parts = path.rstrip(b"/").split(b"/")
        if any(part in {b"", b".", b".."} for part in parts):
            raise SourceError("Invalid Git index path")
        node = tree
        for part in parts[:-1]:
            node = node.setdefault(part, {})
            if not isinstance(node, dict):
                raise SourceError("Conflicting Git index directory and file")
        if parts[-1] in node:
            raise SourceError("Duplicate Git index path")
        node[parts[-1]] = (numeric_mode, oid)

    def identity(node: dict) -> bytes:
        rows = []
        for name, value in node.items():
            if isinstance(value, dict):
                mode, oid = 0o40000, identity(value)
            else:
                mode, oid = value
            order = name + (b"/" if mode == 0o40000 else b"")
            row = format(mode, "o").encode("ascii") + b" " + name + b"\0" + oid
            rows.append((order, row))
        return _object_id(algorithm, b"tree", b"".join(row for _, row in sorted(rows)))

    return identity(tree).hex().encode("ascii") + b"\n"


def _git_grep(root: Path, environment: dict[str, str], arguments: list[str]) -> bytes:
    # Explicit flags, so a user's grep.* configuration cannot change the record
    # shape or the pattern language.
    result = subprocess.run(
        _git_command(environment, "grep", "--cached", "--no-color", "--full-name", "-I", *arguments),
        cwd=root, capture_output=True, check=False, env=environment,
    )
    if result.returncode not in (0, 1):
        raise SourceError(
            f"git grep --cached {' '.join(arguments)} failed: "
            f"{result.stderr.decode(errors='replace').strip()}"
        )
    return result.stdout


def _grep_records(output: bytes) -> Iterable[tuple[str, int, str]]:
    """Read path-NUL-number-NUL-text-LF without splitting a legal LF path."""
    position = 0
    while position < len(output):
        path_end = output.find(b"\0", position)
        number_end = output.find(b"\0", path_end + 1) if path_end >= 0 else -1
        text_end = output.find(b"\n", number_end + 1) if number_end >= 0 else -1
        if path_end == position or min(path_end, number_end, text_end) < 0:
            raise SourceError("Malformed Git grep record")
        encoded_number = output[path_end + 1:number_end]
        if not encoded_number.isdigit() or int(encoded_number) < 1:
            raise SourceError("Malformed Git grep line number")
        yield (
            os.fsdecode(output[position:path_end]),
            int(encoded_number),
            output[number_end + 1:text_end].decode("utf-8"),
        )
        position = text_end + 1


def _staged_text(
    root: Path, pattern: str, containing: tuple[str, ...], environment: dict[str, str]
) -> dict[str, str]:
    pathspecs = [f":(glob){pattern}"]
    if containing:
        needles = [argument for needle in containing for argument in ("-e", needle)]
        listed = _git_grep(root, environment, ["-l", "-z", "--fixed-strings", *needles, "--", *pathspecs])
        pathspecs = [f":(literal){path}" for path in _path_names(listed)]
        if not pathspecs:
            return {}
    # Every line of every selected staged blob, as `path NUL line NUL text`.
    output = _git_grep(root, environment, ["-z", "-n", "--basic-regexp", "-e", "^", "--", *pathspecs])
    lines: dict[str, list[str]] = {}
    for path, _, text in _grep_records(output):
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
    for path, number, text in _grep_records(output):
        if accept(path):
            found.append((path, number, text))
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
        (root / "probe.py").write_text("print(1)\n", encoding="utf-8")
        (root / "docs/probe.md").write_text("# short\n", encoding="utf-8")
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
        (root / "probe.py").write_text('token = "' + 'abc12345' + 'SECRET"\n# bad\u200b\n', encoding="utf-8")
        (root / "docs/probe.md").write_text("# large\n" * 1201, encoding="utf-8")
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
            clean = invoke(owner, root, ["--staged"], own)
            assert clean[0] == 0, "SIO1 own partial index"
            if owner is check_file_limits:
                assert "1201" not in clean[2], "SIO1 clean index has no working-file size warning"
            # SIO1/SIO2 decision table: line-only findings warn with status 0;
            # secrets/hygiene findings retain status 1. Both working and foreign
            # hook modes must still scan the ROOT's bad bytes. Requiring the
            # exact file/count warning catches the old false-empty status 0 even
            # though a line measurement no longer rejects the change.
            for arguments, hook, rule in (
                ([], own, "SIO1 default working bytes"),
                (["--staged"], decoy, "SIO2 foreign hook cannot hide debt"),
            ):
                status, _, diagnostic = invoke(owner, root, arguments, hook)
                if owner is check_file_limits:
                    assert status == 0, rule
                    assert "docs/probe.md" in diagnostic and "1201" in diagnostic, rule
                    assert "warn" in diagnostic.lower(), rule
                else:
                    assert status == 1, rule
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

        # SIO5 grep-record causes: the existing portable/raw/LF path set;
        # UTF-8 body, blank line and line number. Both public readers must
        # preserve the same exact staged path/content, never split a path at LF.
        # | Rule | path/record | Reader | Effect |
        # | G1 | real Git names + UTF-8/blank lines | source_text +/- containing | exact text |
        # | G2 | same real Git names | matching_lines | exact path, line, body |
        # | G3 | truncated/invalid protocol fields | both | SourceError |
        # | G4 | invalid UTF-8 text | both | original UnicodeDecodeError |
        grep_line = "# grep framing 名"
        grep_text = grep_line + "\n\nprint('line-3')\n"
        for name in names:
            (root / name).write_bytes(grep_text.encode("utf-8"))
        git(root, "add", "--", *names)
        with patch.dict(os.environ, environment, clear=True):
            for containing in ((), (grep_line,)):
                selected = source_text(root, "*.py", containing)
                assert {name: selected[name] for name in names} == {
                    name: grep_text for name in names
                }, "SIO5 G1 both source_text modes retain exact raw/LF path and UTF-8 body"
                if containing:
                    assert set(selected) == set(names), "SIO5 G1 containing keeps the exact selection"
            assert matching_lines(root, grep_line, lambda relative: relative in names) == sorted(
                (name, 1, grep_line) for name in names
            ), "SIO5 G2 exact matching_lines path/number/UTF-8 body"
            for wire, failure in [
                (b"unterminated path", SourceError),
                (b"source.py\0x\0line\n", SourceError),
                (b"source.py\0" b"0\0line\n", SourceError),
                (b"\0" b"1\0line\n", SourceError),
                (b"source.py\0" b"1\0unterminated", SourceError),
                (b"source.py\0" b"1\0\xff\n", UnicodeDecodeError),
            ]:
                with patch.dict(globals(), _git_grep=lambda *_, data=wire: data):
                    for reader, arguments in [
                        (source_text, (root, "*.py")),
                        (matching_lines, (root, grep_line, lambda _: True)),
                    ]:
                        try:
                            reader(*arguments)
                        except failure:
                            pass
                        else:
                            raise AssertionError("SIO3 G3/G4 malformed grep record became source")

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
                except (subprocess.CalledProcessError, SourceError) as error:
                    # Outside any checkout Git refuses with 128; a task-owned
                    # TMPDIR inside a checkout can discover the parent, which
                    # the exact-root admission rejects as SourceError. Both
                    # paths must refuse this dangling marker, never disk-walk.
                    if isinstance(error, subprocess.CalledProcessError):
                        assert error.returncode == 128, "SIO3 dangling repository marker"
                    else:
                        assert "not an admitted repository top" in str(error), "SIO3 parent-root refusal"
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
