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
# `@kani key=value ...` states its non-default Kani selection (formal_plan);
# `@proof-input-debt <rule> <kernel> [<parameter>]` records an unreviewed
# proof-input finding (check_proof_input_derivation).
TAGS = ("obligation", "kani", "proof-input-debt")
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
# and, for a source the surface inventories flag, how it is classified:
#
#     // @formal surface requirement=<feature.requirement>   (a product boundary)
#     // @formal surface obligation=<obligation id>          (one line per obligation)
#     // @formal surface exclude=<boundary>                  (not a safety surface)
#     // @formal owner <path>          (exclude only: where the behaviour does live)
#     // @formal reason: <text>        (required; continued on further reason lines)
#
# An anchor line anywhere else in the file is refused, so the block stays the
# one place a reader looks. Consumer: ``formal_ledger`` (evidence and
# production entrypoints of each obligation).
#
# The sources that state their own role: every Rust source under crates/ and
# every Web source under web/src/. One rule serves both sides -- the scan reads
# anchors only here, and an obligation file may not name such a source.
SOURCE_ANCHORED = re.compile(r"(?:crates/.+\.rs|web/src/.+)", re.DOTALL)
HEADER_PREFIX = "// @formal "
HEADER = re.compile(r"^// @formal (?P<role>[a-z][a-z-]*):?(?: (?P<value>\S(?:.*\S)?))?$")
HEADER_ROLES = ("entrypoint", "evidence", "surface", "reason", "owner")
SURFACE_KINDS = ("requirement", "obligation", "exclude")


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


@dataclass(frozen=True)
class Surface:
    """One source's own surface classification.

    Consumers: ``check_formal_surface`` (Rust sources), ``check_formal_web_surface``
    (Web sources) and ``check_proof_boundaries`` (requirement-only surfaces).
    """

    path: str
    line: int
    requirement: str | None
    obligations: tuple[str, ...]
    exclude: str | None
    reason: str
    owners: tuple[str, ...]


def surfaces(found: list[Header]) -> tuple[dict[str, Surface], list[str]]:
    """Each source's classification from its opening lines, and malformed ones."""

    by_path: dict[str, list[Header]] = {}
    for header in found:
        if header.role in ("surface", "reason", "owner"):
            by_path.setdefault(header.path, []).append(header)
    classified: dict[str, Surface] = {}
    errors: list[str] = []
    for path, headers in sorted(by_path.items()):
        kinds: dict[str, list[tuple[Header, str]]] = {}
        for header in headers:
            if header.role != "surface":
                continue
            kind, equals, value = header.value.partition("=")
            if kind not in SURFACE_KINDS or not equals or not value or " " in value:
                errors.append(
                    f"{header.where}: expected `// @formal surface "
                    f"<{'|'.join(SURFACE_KINDS)}>=<value>`, got {header.value!r}"
                )
                continue
            kinds.setdefault(kind, []).append((header, value))
        reason = " ".join(header.value for header in headers if header.role == "reason")
        owners = tuple(header.value for header in headers if header.role == "owner")
        first = min(headers, key=lambda header: header.line)
        if not kinds:
            errors.append(f"{first.where}: a @formal reason or owner line needs a `// @formal surface` line")
            continue
        if len(kinds) != 1 or any(len(values) > 1 for kind, values in kinds.items() if kind != "obligation"):
            errors.append(
                f"{first.where}: duplicate surface classification {path}: a source is classified once, "
                "by one requirement=, one exclude=, or obligation= lines"
            )
            continue
        (kind, values), = kinds.items()
        names = [value for _, value in values]
        if len(names) != len(set(names)):
            errors.append(f"{first.where}: surface classification {path} repeats an obligation id")
            continue
        if not reason:
            errors.append(f"{first.where}: surface classification {path} needs a `// @formal reason:` line")
            continue
        if owners and kind != "exclude":
            errors.append(f"{first.where}: `// @formal owner` lines belong to an exclude= surface only")
            continue
        classified[path] = Surface(
            path, first.line,
            values[0][1] if kind == "requirement" else None,
            tuple(names) if kind == "obligation" else (),
            values[0][1] if kind == "exclude" else None,
            reason, owners,
        )
    return classified, errors
