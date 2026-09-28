"""What the Rust sources declare about formal obligations.

A Kani harness names the obligations it serves in its own doc comment, one line
per obligation, directly beside ``#[kani::proof]``::

    #[cfg(kani)]
    /// @obligation work.persisted_epoch_is_exact_nonnegative_and_nonwrapping
    #[kani::proof]
    fn work_lease_epoch_is_exact_nonnegative_and_nonwrapping() { ... }

A harness whose Kani selection is not the default states it the same way, in
one line: ``/// @kani profile=<name> features=<f,...> tier=pr|nightly`` (see
``formal_plan``).

The harness block is the run of outer attributes and line comments directly
above the ``fn`` (through ``#[kani::proof]`` and any attribute after it). An
anchor anywhere else is dangling. This module only reads and parses; the rules
that make an anchor an error live with the ledger that consumes them
(``formal_ledger.load``).

Consumers: ``formal_ledger`` (assembly of ``proof_harnesses`` and harness-file
evidence) and ``formal_plan`` (the Kani batches of every harness).
"""

from __future__ import annotations

import re
import tomllib
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

from _crate_boundary_workspace import rust_syntax
from formal_sources import matching_lines, source_text

# A scan reads every Rust source (anchors and harnesses) and every Cargo
# manifest (the package that owns a harness); the entry points that scan
# (formal_ledger and the gates that load it) declare both in INPUT_GLOBS.
RUST_SOURCES = "crates/**/*.rs"
CARGO_MANIFESTS = "crates/**/Cargo.toml"
PROOF_ATTRIBUTE = "#[kani::proof]"
# `@obligation <id>` names an obligation the harness serves (formal_ledger);
# `@kani key=value ...` states its non-default Kani selection (formal_plan).
TAGS = ("obligation", "kani")
# A doc-comment line carrying one tag: `/// @<tag> <value...>`.
ANCHOR = re.compile(r"^\s*///\s*@(?P<tag>[a-z][a-z-]*)(?:\s+(?P<value>.*?))?\s*$")
BLOCK_LINE = re.compile(r"^\s*(?:#\[.*\]|///.*|//(?!!).*)\s*$")
FUNCTION = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)
OBLIGATION_ID = re.compile(r"[a-z0-9_]+\.[a-z0-9_]+")


@dataclass(frozen=True)
class Anchor:
    path: str
    line: int
    tag: str
    value: str

    @property
    def where(self) -> str:
        return f"{self.path}:{self.line}"


@dataclass
class Harness:
    """One ``#[kani::proof]`` function and the anchors in its block."""

    name: str
    path: str
    line: int
    package: str | None
    anchors: list[Anchor] = field(default_factory=list)

    @property
    def where(self) -> str:
        return f"{self.path}:{self.line}"

    def values(self, tag: str) -> list[Anchor]:
        return [anchor for anchor in self.anchors if anchor.tag == tag]


@dataclass
class Scan:
    harnesses: list[Harness]
    # Anchors outside every harness block, and structural errors (file:line).
    dangling: list[Anchor]
    errors: list[str]


def _packages(root: Path) -> dict[str, str]:
    """Crate directory -> Cargo package name."""

    packages: dict[str, str] = {}
    for relative, text in source_text(root, CARGO_MANIFESTS).items():
        try:
            package = tomllib.loads(text).get("package", {})
        except tomllib.TOMLDecodeError:
            continue
        if isinstance(package.get("name"), str):
            packages[PurePosixPath(relative).parent.as_posix()] = package["name"]
    return packages


def _owner(relative: str, packages: dict[str, str]) -> str | None:
    for parent in PurePosixPath(relative).parents:
        if parent.as_posix() in packages:
            return packages[parent.as_posix()]
    return None


def scan_text(relative: str, text: str, package: str | None) -> Scan:
    """Harnesses, dangling anchors and structural errors of one Rust source."""

    raw = text.split("\n")
    syntax = rust_syntax(text).split("\n")
    harnesses: list[Harness] = []
    errors: list[str] = []
    claimed: set[int] = set()
    for index, line in enumerate(syntax):
        if line.strip() != PROOF_ATTRIBUTE:
            continue
        end = index + 1
        while end < len(raw) and BLOCK_LINE.match(raw[end]) and not FUNCTION.match(raw[end]):
            end += 1
        function = FUNCTION.match(raw[end]) if end < len(raw) else None
        if function is None:
            errors.append(f"{relative}:{index + 1}: {PROOF_ATTRIBUTE} is not followed by a fn")
            continue
        start = index
        while start > 0 and BLOCK_LINE.match(raw[start - 1]):
            start -= 1
        harness = Harness(function["name"], relative, end + 1, package)
        for number in range(start, end):
            anchor = ANCHOR.match(raw[number])
            if anchor is not None:
                claimed.add(number)
                harness.anchors.append(
                    Anchor(relative, number + 1, anchor["tag"], anchor["value"] or "")
                )
        harnesses.append(harness)
    dangling = [
        Anchor(relative, number + 1, match["tag"], match["value"] or "")
        for number, line in enumerate(raw)
        if number not in claimed and (match := ANCHOR.match(line)) is not None and match["tag"] in TAGS
    ]
    return Scan(harnesses, dangling, errors)


def scan(root: Path) -> Scan:
    """Every harness and every anchor under ``crates/``."""

    packages = _packages(root)
    needles = (PROOF_ATTRIBUTE, *(f"@{tag}" for tag in TAGS))
    result = Scan([], [], [])
    for relative, text in source_text(root, RUST_SOURCES, needles).items():
        found = scan_text(relative, text, _owner(relative, packages))
        result.harnesses.extend(found.harnesses)
        result.dangling.extend(found.dangling)
        result.errors.extend(found.errors)
    return result


# --------------------------------------------------------------------------
# File-head anchors.
#
# A Rust or Web source states what it is to the ledger in its own first lines,
# one fact per line, before anything else in the file:
#
#     // @formal entrypoint <obligation id>   (production entrypoint, so evidence too)
#     // @formal evidence <obligation id>     (evidence only: a test, a model link)
#
# An anchor line anywhere else in the file is refused, so the block stays the
# one place a reader looks. Consumer: ``formal_ledger`` (evidence and
# production entrypoints of each obligation).
#
# The sources that state their own role: every Rust source under crates/ and
# every Web source under web/src/. One rule serves both sides -- the scan reads
# anchors only here, and an obligation file may not name such a source.
SOURCE_ANCHORED = re.compile(r"(?:crates/.+\.rs|web/src/.+)")
HEADER_PREFIX = "// @formal "
HEADER = re.compile(r"^// @formal (?P<role>[a-z][a-z-]*)(?: (?P<value>\S(?:.*\S)?))?$")
HEADER_ROLES = ("entrypoint", "evidence")


@dataclass(frozen=True)
class Header:
    path: str
    line: int
    role: str
    value: str

    @property
    def where(self) -> str:
        return f"{self.path}:{self.line}"


def headers(root: Path) -> tuple[list[Header], list[str]]:
    """Every file-head anchor under the header sources, and every misplaced one."""

    found: list[Header] = []
    errors: list[str] = []
    by_path: dict[str, list[tuple[int, str]]] = {}
    for path, number, text in matching_lines(root, HEADER_PREFIX.strip(), SOURCE_ANCHORED.fullmatch):
        if text.lstrip().startswith(HEADER_PREFIX):
            by_path.setdefault(path, []).append((number, text))
    for path, lines in sorted(by_path.items()):
        numbers = [number for number, _ in lines]
        for number, text in lines:
            where = f"{path}:{number}"
            match = HEADER.match(text.rstrip("\r"))
            if number > len(numbers) or numbers[: number] != list(range(1, number + 1)):
                errors.append(f"{where}: a @formal anchor belongs in the file's opening lines, before any code")
            elif match is None or match["role"] not in HEADER_ROLES or not match["value"]:
                errors.append(
                    f"{where}: expected `// @formal <{'|'.join(HEADER_ROLES)}> <value>`, got {text.strip()!r}"
                )
            else:
                found.append(Header(path, number, match["role"], match["value"]))
    return found, errors
