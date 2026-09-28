"""Which bytes the formal gates read: the index in a repository, the disk otherwise.

The obligation ledger is assembled from TOML files and from anchors written in
Rust sources. Both are read here, and only here, so every gate judges the same
bytes. In a Git work tree that is the index (``git grep --cached``): the staged
truth a pre-commit hook judges and a clean CI checkout holds, so an edit is seen
once it is staged. Outside a repository -- a checker's fixture tree -- the files
on disk are read.

Consumers: ``formal_ledger`` (TOML storage), ``formal_anchors`` (Rust anchors)
and the fixtures of their self-tests.
"""

from __future__ import annotations

import os
import subprocess
from collections.abc import Callable
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
    if exported is not None and Path(exported).resolve() == Path(lines[1]).resolve():
        return dict(os.environ)
    return clean


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
