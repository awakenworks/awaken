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


class GitBlobReader:
    """``git show <revision>:<path>`` bytes for many paths from one Git process.

    ``git_output`` rediscovers the repository and starts Git on every read: two
    processes per file. A qualifier reading a manifest closure paid that for
    each manifest, and under heavy IO those short processes, not the reads,
    took the time. This reader discovers ``root`` once and serves every blob
    from one ``git cat-file --batch``, whose bytes are the raw blob exactly as
    ``git show`` prints it (neither applies textconv, filters or EOL rules).

    Anything that is not a found blob -- a missing path, an ambiguous name, a
    tree, a gitlink, a name the line protocol cannot carry -- is re-read with
    ``git show``, so its bytes or its Git failure stay exactly the original
    ones. Consumers: the SDK compiler-input source observation.
    """

    def __init__(self, root: Path):
        self.root = root
        self.environment = _source_environment(root)
        self._process: subprocess.Popen | None = None

    def __enter__(self) -> GitBlobReader:
        return self

    def __exit__(self, kind: object, *exception: object) -> None:
        if kind is None:
            self.close()
        elif self._process is not None:
            # The first failure stays the reported one; retire Git quietly.
            self._failure()

    def output(self, *arguments: str) -> bytes:
        """``git_output`` for this root, without rediscovering it."""
        return _git_output(self.root, self.environment, *arguments)

    def paths(self, *arguments: str) -> list[str]:
        """``git_paths`` for this root, without rediscovering it."""
        return _path_names(self.output(*arguments))

    def show(self, revision: str, path: str) -> bytes:
        name = f"{revision}:{path}"
        encoded = os.fsencode(name)
        if any(character in encoded for character in (b"\n", b"\r", b"\0")):
            return self.output("show", name)
        process = self._batch()
        assert process.stdin is not None and process.stdout is not None
        try:
            process.stdin.write(encoded + b"\n")
            process.stdin.flush()
            header = process.stdout.readline()
        except OSError as error:
            raise self._failure() from error
        if not header.endswith(b"\n"):
            raise self._failure()
        fields = header[:-1].split(b" ")
        if (len(fields) == 3 and re.fullmatch(rb"[a-f0-9]{40}|[a-f0-9]{64}", fields[0])
                and fields[2].isdigit()):
            size = int(fields[2])
            body = process.stdout.read(size + 1)
            if len(body) != size + 1 or not body.endswith(b"\n"):
                raise self._failure()
            if fields[1] == b"blob":
                return body[:-1]
        # Not a blob Git found: keep the original reader's bytes or failure.
        return self.output("show", name)

    def _batch(self) -> subprocess.Popen:
        if self._process is None:
            self._process = subprocess.Popen(
                _git_command(self.environment, "-c", "core.fsmonitor=false", "-c", "diff.autoRefreshIndex=false",
                             "-C", str(self.root), "cat-file", "--batch"),
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=self.environment,
            )
        return self._process

    def _failure(self) -> subprocess.CalledProcessError:
        """A broken batch protocol; the process is retired, never reused."""
        process, self._process = self._process, None
        status = 1
        if process is not None:
            process.kill()
            status = process.wait() or 1
        return subprocess.CalledProcessError(status, ["git", "cat-file", "--batch"])

    def close(self) -> None:
        process, self._process = self._process, None
        if process is None:
            return
        assert process.stdin is not None and process.stdout is not None
        process.stdin.close()
        process.stdout.read()
        process.stdout.close()
        if process.wait():
            raise subprocess.CalledProcessError(process.returncode, ["git", "cat-file", "--batch"])


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
