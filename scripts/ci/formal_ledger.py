"""The formal obligation ledger, assembled from its sources.

Every gate that reasons about formal obligations -- the coverage, surface,
feature, lane-reachability and proof-input checkers, the verified-profile
qualification and the release manifest -- consumes one assembled value, the
coverage ledger in its v1 shape:

    {metric, minimum_ratio, verified_profile, obligations: [...]}

Each obligation row carries ``id``, ``formalizable``, ``status``, ``evidence``,
``proof_harnesses``, ``claim_scope``, ``proof_grade``, ``bounds``,
``tcb_assumptions`` and ``production_entrypoints``.

Storage. The assembled value is never committed. It is built from

* ``formal/coverage.toml`` -- the metric, the minimum ratio and the verified
  profile, and
* ``formal/obligations/<prefix>/<name>.toml`` -- one file per obligation. The
  path *is* the id (``<prefix>.<name>``); ``formalizable`` is derived from
  ``claim_scope``; every array holds one value per line and every bound is its
  own ``[[bounds]]`` table, so two branches that touch different obligations,
  or different values of one, merge without conflict.

Which bytes. In a Git work tree the ledger is read from the index (``git grep
--cached``), the same staged truth a pre-commit hook judges and a clean CI
checkout holds; an edit is seen by the gates once it is staged. Outside a
repository -- a checker's fixture tree -- the files on disk are read.

``export`` writes the assembled v1 JSON under ``target/`` for a reader that
wants one file; nothing may commit it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Input set: the profile and every obligation file. Each gate that loads the
# ledger spells the same two globs in its own INPUT_GLOBS.
INPUT_GLOBS: tuple[str, ...] = ("formal/coverage.toml", "formal/obligations/**/*")

# Consumers of these names: ``load``/``files``/``write`` below.
PROFILE_FILE = "formal/coverage.toml"
OBLIGATION_DIRECTORY = "formal/obligations"

OBLIGATION_PATH = re.compile(
    r"formal/obligations/(?P<prefix>[a-z0-9_]+)/(?P<name>[a-z0-9_]+)\.toml"
)
SCALAR_FIELDS = ("claim_scope", "proof_grade", "status")
ARRAY_FIELDS = ("tcb_assumptions", "evidence", "proof_harnesses", "production_entrypoints")
PROFILE_KEYS = ("metric", "minimum_ratio", "verified_profile")


class LedgerError(ValueError):
    """The stored ledger cannot be assembled into the v1 shape."""


# --------------------------------------------------------------------------
# Reading: staged truth in a repository, the files on disk otherwise.


# Git exports these to a hook. They name the repository being committed, not
# whichever directory a checker happens to read -- a fixture tree under /tmp
# would otherwise be judged against the hook's repository and index.
_REPOSITORY_ENVIRONMENT = (
    "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX", "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
)


def detached_git_environment() -> dict[str, str]:
    """The environment with every repository-selecting Git variable removed.

    Consumers: repository discovery below, and fixtures that run Git in a
    temporary tree while a hook's variables are exported.
    """

    return {key: value for key, value in os.environ.items() if key not in _REPOSITORY_ENVIRONMENT}


def _repository_environment(root: Path) -> dict[str, str] | None:
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


def _staged_text(root: Path, pattern: str, environment: dict[str, str]) -> dict[str, str]:
    """Every staged text file under ``pattern`` (a pathlib-style glob)."""

    # Every line of every matching staged blob, as `path NUL line NUL text`.
    # The flags are explicit so a user's grep.* configuration cannot change
    # the record shape or the pattern language.
    result = subprocess.run(
        ["git", "grep", "--cached", "--no-color", "--basic-regexp", "-I", "-z", "-n",
         "--full-name", "-e", "^", "--", f":(glob){pattern}"],
        cwd=root, capture_output=True, check=False, env=environment,
    )
    if result.returncode not in (0, 1):
        raise LedgerError(
            f"git grep --cached {pattern} failed: {result.stderr.decode(errors='replace').strip()}"
        )
    lines: dict[str, list[str]] = {}
    for record in result.stdout.decode("utf-8").split("\n")[:-1]:
        path, _, rest = record.partition("\0")
        _, _, text = rest.partition("\0")
        lines.setdefault(path, []).append(text + "\n")
    return {path: "".join(body) for path, body in lines.items()}


def _worktree_text(root: Path, pattern: str) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
        for path in sorted(root.glob(pattern))
        if path.is_file()
    }


def source_text(root: Path, pattern: str) -> dict[str, str]:
    """The text of every file under ``pattern``: staged in a repository, on disk otherwise.

    Consumers: ``load`` and the source-anchor scans that assemble the ledger.
    """

    environment = _repository_environment(root)
    if environment is None:
        return _worktree_text(root, pattern)
    return _staged_text(root, pattern, environment)


# --------------------------------------------------------------------------
# Assembly.


def _string_list(value: object, where: str) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) and item for item in value):
        raise LedgerError(f"{where} must be a list of non-empty strings")
    if len(value) != len(set(value)):
        raise LedgerError(f"{where} repeats a value")
    return list(value)


def _obligation(relative: str, text: str) -> dict:
    match = OBLIGATION_PATH.fullmatch(relative)
    if match is None:
        raise LedgerError(
            f"{relative}: an obligation file is formal/obligations/<prefix>/<name>.toml "
            "in lower_snake_case; its path is its id"
        )
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise LedgerError(f"{relative}: {error}") from error
    unknown = sorted(set(document) - {*SCALAR_FIELDS, *ARRAY_FIELDS, "bounds"})
    if unknown:
        raise LedgerError(f"{relative}: unknown field(s) {', '.join(unknown)}")
    row: dict = {"id": f"{match['prefix']}.{match['name']}"}
    for key in SCALAR_FIELDS:
        if key in document:
            if not isinstance(document[key], str):
                raise LedgerError(f"{relative}: {key} must be a string")
            row[key] = document[key]
    # The ledger's own rule, stated once: only repository-safety claims are
    # formalizable. Storing both would let them disagree.
    row["formalizable"] = row.get("claim_scope") == "repository_safety"
    for key in ARRAY_FIELDS:
        if key in document:
            row[key] = _string_list(document[key], f"{relative}: {key}")
    if "bounds" in document:
        tables = document["bounds"]
        if not isinstance(tables, list) or not all(
            isinstance(table, dict) and set(table) == {"text"} and isinstance(table["text"], str)
            for table in tables
        ):
            raise LedgerError(f"{relative}: every [[bounds]] table holds exactly one text string")
        row["bounds"] = [table["text"] for table in tables]
    return row


def _profile(text: str | None) -> dict:
    if text is None:
        return {}
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise LedgerError(f"{PROFILE_FILE}: {error}") from error
    unknown = sorted(set(document) - set(PROFILE_KEYS))
    if unknown:
        raise LedgerError(f"{PROFILE_FILE}: unknown field(s) {', '.join(unknown)}")
    return document


def present(root: Path) -> bool:
    """Does ``root`` carry an obligation ledger at all?"""

    return bool(source_text(root, f"{OBLIGATION_DIRECTORY}/**/*")) or bool(
        source_text(root, PROFILE_FILE)
    )


def load(root: Path) -> dict:
    """Assemble the coverage ledger of ``root`` in its v1 shape."""

    profile = source_text(root, PROFILE_FILE)
    obligations = source_text(root, f"{OBLIGATION_DIRECTORY}/**/*")
    ledger = _profile(profile.get(PROFILE_FILE))
    ledger["obligations"] = [
        _obligation(relative, text) for relative, text in sorted(obligations.items())
    ]
    return ledger


# --------------------------------------------------------------------------
# Writing (fixtures, the migration) and digests.


def _basic(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def _bound(text: str) -> str:
    literal_safe = (
        "'''" not in text
        and not text.startswith("\n")
        and not text.endswith("'''")
        and all(ord(character) >= 32 or character == "\t" for character in text)
    )
    return f"'''{text}'''" if literal_safe else _basic(text)


def render_obligation(row: dict) -> str:
    lines: list[str] = []
    for key in SCALAR_FIELDS:
        if key in row:
            lines.append(f"{key} = {_basic(row[key])}")
    for key in ARRAY_FIELDS:
        if key not in row:
            continue
        if not row[key]:
            lines.append(f"{key} = []")
            continue
        lines.append(f"{key} = [")
        lines.extend(f"  {_basic(value)}," for value in row[key])
        lines.append("]")
    for text in row.get("bounds", []):
        lines.extend(["", "[[bounds]]", f"text = {_bound(text)}"])
    return "\n".join(lines) + "\n"


def render_profile(ledger: dict) -> str:
    lines: list[str] = []
    if "metric" in ledger:
        lines.append(f"metric = {_basic(ledger['metric'])}")
    if "minimum_ratio" in ledger:
        lines.append(f"minimum_ratio = {json.dumps(ledger['minimum_ratio'])}")
    profile = ledger.get("verified_profile")
    if profile is not None:
        lines.extend(["", "[verified_profile]"])
        for key, value in profile.items():
            if isinstance(value, list):
                lines.append(f"{key} = [")
                lines.extend(f"  {_basic(item)}," for item in value)
                lines.append("]")
            elif isinstance(value, str):
                lines.append(f"{key} = {_basic(value)}")
            else:
                lines.append(f"{key} = {json.dumps(value)}")
    return "\n".join(lines) + "\n"


def files(ledger: dict) -> dict[str, str]:
    """The stored form of an assembled ledger: repository path -> text."""

    stored = {PROFILE_FILE: render_profile(ledger)}
    for row in ledger["obligations"]:
        prefix, dot, name = str(row["id"]).partition(".")
        relative = f"{OBLIGATION_DIRECTORY}/{prefix}/{name}.toml"
        if not dot or OBLIGATION_PATH.fullmatch(relative) is None:
            raise LedgerError(f"obligation id {row['id']!r} is not <prefix>.<name> in lower_snake_case")
        if relative in stored:
            raise LedgerError(f"duplicate obligation id {row['id']}")
        stored[relative] = render_obligation(row)
    return stored


def write(root: Path, ledger: dict) -> None:
    """Replace the stored ledger under ``root`` with ``ledger`` (v1 shape).

    Consumers: checker self-tests and mutation cases that build a fixture tree,
    so a fixture follows the storage instead of hard-coding it.
    """

    directory = root / OBLIGATION_DIRECTORY
    if directory.is_dir():
        for path in sorted(directory.glob("**/*.toml")):
            path.unlink()
    for relative, text in files(ledger).items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def canonical_bytes(ledger: dict) -> bytes:
    """Order-independent bytes of an assembled ledger.

    Obligations are ordered by id; every other list keeps its order, because a
    list's order is part of what was declared.
    """

    ordered = dict(ledger)
    ordered["obligations"] = sorted(ledger["obligations"], key=lambda row: row["id"])
    return (json.dumps(ordered, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def digest(ledger: dict) -> str:
    return "sha256:" + hashlib.sha256(canonical_bytes(ledger)).hexdigest()


def self_test() -> None:
    """Decision table for the storage and the reader.

    Causes: C1 a ledger is written and read back; C2 an obligation file sits
    at another depth or spelling; C3 it names an unknown field; C4 an array
    repeats a value; C5 two rows claim one id; C6 claim_scope decides
    formalizable; C7 in a repository the staged and the on-disk bytes differ;
    C8 the same tree is not a repository; C9 a hook exported this repository
    with the index being committed; C10 a hook exported another repository.
    Effects: E1 load returns exactly what was written (canonically); E2 the
    reader refuses by path; E3 load reads the staged bytes; E4 load reads disk;
    E5 load reads the hook's index.
    Rules: C1 => E1; C2|C3|C4|C5 => E2; C6 => formalizable == (scope ==
    repository_safety); C7 => E3; C8 => E4; C9 => E5; C10 => E3.
    """

    import shutil  # noqa: PLC0415 - only the self-test builds fixtures
    import tempfile  # noqa: PLC0415
    from unittest import mock  # noqa: PLC0415

    rows = [
        {"id": "run.safe", "claim_scope": "repository_safety", "proof_grade": "production_kernel_proved",
         "status": "kernel_proved", "tcb_assumptions": [], "evidence": ["crates/a/src/lib.rs"],
         "proof_harnesses": ["safe"], "production_entrypoints": ["crates/a/src/lib.rs"],
         "bounds": ["one ' quote and a trailing quote'", "two"]},
        {"id": "run.external", "claim_scope": "external_assumption", "proof_grade": "external_tcb",
         "status": "external", "tcb_assumptions": ["external.x"], "evidence": ["docs/x.md"],
         "production_entrypoints": [], "bounds": ["text with " + "'" * 3 + "triple quotes"]},
    ]
    ledger = {"metric": "m", "minimum_ratio": 0.7, "verified_profile": {"schema_version": 1, "id": "v"},
              "obligations": rows}
    expected = {**ledger, "obligations": [
        {**row, "formalizable": row["claim_scope"] == "repository_safety"} for row in rows
    ]}

    def refused(root: Path, fragment: str) -> None:
        try:
            load(root)
        except LedgerError as error:
            assert fragment in str(error), (fragment, str(error))
        else:
            raise AssertionError(f"load accepted a ledger that must be refused: {fragment}")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        write(root, ledger)
        assert canonical_bytes(load(root)) == canonical_bytes(expected), "C1/E1 and C6"
        assert present(root) and not present(root / "formal"), "presence"
        stray = root / OBLIGATION_DIRECTORY / "run" / "deep" / "x.toml"
        stray.parent.mkdir()
        stray.write_text('status = "external"\n')
        refused(root, "formal/obligations/run/deep/x.toml")
        stray.unlink()
        upper = root / OBLIGATION_DIRECTORY / "run" / "Upper.toml"
        upper.write_text('status = "external"\n')
        refused(root, "run/Upper.toml")
        upper.unlink()
        safe = root / OBLIGATION_DIRECTORY / "run" / "safe.toml"
        original = safe.read_text()
        safe.write_text("formalizable = true\n" + original)
        refused(root, "unknown field(s) formalizable")
        repeated = '"crates/a/src/lib.rs",\n  "crates/a/src/lib.rs",\n]'
        safe.write_text(original.replace('"crates/a/src/lib.rs",\n]', repeated, 1))
        refused(root, "evidence repeats a value")
        safe.write_text(original)
        try:
            files({"obligations": [rows[0], dict(rows[0])]})
        except LedgerError as error:
            assert "duplicate obligation id run.safe" in str(error), "C5/E2"
        else:
            raise AssertionError("C5/E2 two rows with one id were stored")

        git = ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
               "-c", "core.hooksPath=/dev/null"]
        # A hook exports its own repository; the fixture must never touch it.
        detached = detached_git_environment()
        for arguments in (["init", "--quiet"], ["add", "."], ["commit", "--quiet", "-m", "fixture"]):
            subprocess.run([*git, *arguments], cwd=root, check=True, capture_output=True, env=detached)
        safe.write_text(original.replace('status = "kernel_proved"', 'status = "modeled_only"'))
        staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "kernel_proved", "C7/E3 unstaged bytes are not the ledger"
        subprocess.run([*git, "add", "."], cwd=root, check=True, capture_output=True, env=detached)
        staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "modeled_only", "C7/E3 staged bytes are the ledger"
        # C9 a hook exports this repository and the separate index being
        # committed (`git commit <path>`): that index is the staged truth.
        git_dir = root / ".git"
        partial = git_dir / "partial-index"
        subprocess.run([*git, "read-tree", "HEAD"], cwd=root, check=True, capture_output=True,
                       env={**detached, "GIT_INDEX_FILE": str(partial)})
        with mock.patch.dict(os.environ, {"GIT_DIR": str(git_dir), "GIT_INDEX_FILE": str(partial)}):
            staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "kernel_proved", "C9 the hook's own index"
        # C10 a hook exports another repository: this tree keeps its own index.
        with mock.patch.dict(os.environ, {"GIT_DIR": str(root / "elsewhere.git"), "GIT_INDEX_FILE": str(partial)}):
            staged = {row["id"]: row for row in load(root)["obligations"]}
        assert staged["run.safe"]["status"] == "modeled_only", "C10 a foreign hook repository"
        shutil.rmtree(root / ".git")
        safe.write_text(original)
        on_disk = {row["id"]: row for row in load(root)["obligations"]}
        assert on_disk["run.safe"]["status"] == "kernel_proved", "C8/E4"
    print("formal_ledger self-test: the decision table holds")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--self-test", action="store_true", help="run the decision table")
    action.add_argument(
        "--export", type=Path, nargs="?", const=Path("target/formal/coverage.json"), metavar="PATH",
        help="write the assembled v1 ledger (under target/ only; it is never committed)",
    )
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        self_test()
        return 0
    root = Path(__file__).resolve().parents[2]
    output = arguments.export if arguments.export.is_absolute() else root / arguments.export
    if (root / "target") not in output.resolve().parents:
        parser.error("--export writes under target/ only; the assembled ledger is never committed")
    try:
        ledger = load(root)
    except LedgerError as error:
        print(f"formal ledger: {error}", file=sys.stderr)
        return 1
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(ledger, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"formal ledger: {len(ledger['obligations'])} obligations -> {output} ({digest(ledger)})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
