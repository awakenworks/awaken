#!/usr/bin/env python3
"""Derive the Kani plan from the harnesses themselves.

Every ``#[kani::proof]`` function is a harness; its package is the Cargo package
that owns its file, and its Kani selection is the default unless its own block
says otherwise in one tag line::

    /// @kani profile=<name> features=<f,...> tier=pr|nightly

Keys (each optional, each at most once):

* ``profile``  -- an audited solver/loop profile in ``formal_kani_options``;
* ``features`` -- Cargo features the harness needs (comma-separated);
* ``default-features=false`` -- build the package without default features;
* ``target=lib`` -- select only the library target (a package whose binary
  compiles the same module would otherwise present the harness twice);
* ``tier``     -- ``pr`` (default) or ``nightly``, a harness measured too slow
  for the per-change lane. The full tier runs both.

Harnesses sharing (package, features, default-features, target, profile) are
sorted and divided into bounded batches. Each batch is one ``cargo kani``
invocation and one CBMC resource boundary. Every harness is selected with
``--exact`` by its fully qualified name, derived from the module tree of its
Cargo target, so no filter can match a second harness.

``kani --tier pr|full`` prints one batch per line (``package argv...``) for the
formal gate's loop. Nothing is listed by hand, so a new harness is planned by
existing and a moved one follows its file.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

import formal_anchors
from _crate_boundary_workspace import _balanced_end, rust_syntax
from formal_kani_options import serialize
from formal_sources import SourceError, detached_git_environment, repository_environment, source_text

# Input set: every Rust source (harnesses, their tags and the module tree that
# names them) and every Cargo manifest (package and targets).
INPUT_GLOBS: tuple[str, ...] = ("crates/**/*.rs", "crates/**/Cargo.toml")

# The 23-harness event-admission selection exceeded the gate's 1800s deadline
# although its individual proofs converged. Eight consecutive proofs took at
# most 977s in that run; keep the same per-batch budget and bound aggregation.
MAX_HARNESSES_PER_BATCH = 8

ROOT = Path(__file__).resolve().parents[2]
TIERS = ("pr", "nightly")
FEATURE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]*")
ITEM = re.compile(
    r"(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?\bmod\s+(?P<module>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<delimiter>[;{])"
    r"|\binclude!\s*\("
    r"|#\[kani::proof\]"
)
PROOF_FUNCTION = re.compile(
    r"\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe)\s+)*fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)
PATH_ATTRIBUTE = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]')


class PlanError(ValueError):
    """The harnesses cannot be planned; each line names a source location."""


@dataclass(frozen=True, order=True)
class Selection:
    """What one ``cargo kani`` invocation selects besides its harnesses."""

    package: str
    features: tuple[str, ...] = ()
    default_features: bool = True
    target: str = ""
    profile: str = ""


@dataclass
class Planned:
    harness: formal_anchors.Harness
    selection: Selection
    tier: str
    qualified: tuple[str, ...]


@dataclass
class Batch:
    selection: Selection
    names: list[str] = field(default_factory=list)

    def argv(self) -> list[str]:
        selection = self.selection
        cargo = (["--lib"] if selection.target == "lib" else []) + (
            [] if selection.default_features else ["--no-default-features"]
        ) + (["--features", ",".join(selection.features)] if selection.features else [])
        filters = ["--exact", *(piece for name in self.names for piece in ("--harness", name))]
        options = serialize(selection.profile, selection.package, filters) if selection.profile else filters
        return [selection.package, *cargo, *options]


# --------------------------------------------------------------------------
# Tags.


def _tag(harness: formal_anchors.Harness, errors: list[str]) -> tuple[dict[str, str], str]:
    tags = harness.values("kani")
    if len(tags) > 1:
        errors.append(f"{tags[1].where}: a harness carries at most one @kani line ({harness.name})")
    values: dict[str, str] = {}
    where = tags[0].where if tags else harness.where
    if tags and not tags[0].value.split():
        errors.append(f"{where}: an empty @kani line states nothing; remove it")
    for token in (tags[0].value.split() if tags else []):
        key, equals, value = token.partition("=")
        if not equals or not value:
            errors.append(f"{where}: @kani expects key=value, got {token!r}")
        elif key in values:
            errors.append(f"{where}: @kani repeats {key}")
        elif key not in {"profile", "features", "default-features", "target", "tier"}:
            errors.append(f"{where}: @kani has unknown key {key!r}")
        else:
            values[key] = value
    return values, where


def _selection(harness: formal_anchors.Harness, errors: list[str]) -> tuple[Selection, str]:
    values, where = _tag(harness, errors)
    features = tuple(values["features"].split(",")) if "features" in values else ()
    if any(FEATURE.fullmatch(feature) is None for feature in features) or len(set(features)) != len(features):
        errors.append(f"{where}: @kani features must be distinct Cargo feature names, got {values['features']!r}")
    if values.get("default-features", "false") != "false":
        errors.append(f"{where}: @kani default-features admits only false (true is the default)")
    if values.get("target", "lib") != "lib":
        errors.append(f"{where}: @kani target admits only lib")
    tier = values.get("tier", "pr")
    if tier not in TIERS:
        errors.append(f"{where}: @kani tier must be one of {', '.join(TIERS)}")
    profile = values.get("profile", "")
    if harness.package is None:
        errors.append(f"{harness.where}: Kani harness {harness.name} has no owning Cargo package")
    elif profile:
        try:
            serialize(profile, harness.package, [])
        except ValueError as error:
            errors.append(f"{where}: {error}")
    return Selection(
        harness.package or "", tuple(sorted(features)), "default-features" not in values,
        values.get("target", ""), profile,
    ), tier


# --------------------------------------------------------------------------
# Qualified names: the module tree of each Cargo target, from its sources.


def _targets(package_dir: str, manifest: dict, sources: dict[str, str]) -> dict[str, str]:
    """Target ("lib" or "bin:<root>") -> root source of that Cargo target."""

    package = manifest.get("package", {})
    targets: dict[str, str] = {}
    library_path = f"{package_dir}/{manifest.get('lib', {}).get('path', 'src/lib.rs')}"
    if ("lib" in manifest or package.get("autolib", True)) and library_path in sources:
        targets["lib"] = library_path
    binaries = [f"{package_dir}/{binary['path']}" for binary in manifest.get("bin", []) if "path" in binary]
    for binary in manifest.get("bin", []):
        if "path" not in binary and (name := binary.get("name")):
            binaries += [f"{package_dir}/src/bin/{name}.rs", f"{package_dir}/src/bin/{name}/main.rs"]
            if name == package.get("name"):
                binaries.append(f"{package_dir}/src/main.rs")
    if package.get("autobins", True):
        binaries.append(f"{package_dir}/src/main.rs")
        binaries += [
            path for path in sources
            if re.fullmatch(re.escape(package_dir) + r"/src/bin/(?:[^/]+\.rs|[^/]+/main\.rs)", path)
        ]
    for path in binaries:
        if path in sources:
            targets[f"bin:{path}"] = path
    return targets


def _normal(path: str) -> str:
    parts: list[str] = []
    for part in PurePosixPath(path).parts:
        if part == "..":
            if parts:
                parts.pop()
        elif part != ".":
            parts.append(part)
    return "/".join(parts)


def _module_dir(path: str) -> str:
    pure = PurePosixPath(path)
    return str(pure.parent) if pure.name in {"lib.rs", "main.rs", "mod.rs"} else str(pure.with_suffix(""))


def qualified_names(
    package_dir: str, manifest: dict, sources: dict[str, str]
) -> dict[tuple[str, str], dict[str, str]]:
    """(file, harness name) -> {target: fully qualified Kani harness name}.

    Follows the same module edges the compiler does -- ``mod x;`` (with or
    without ``#[path]``), inline ``mod x { }`` and ``include!("...")`` -- from
    each Cargo target root. A harness is named only at item depth of its
    module; a proof nested in a function body is not a module-level harness.
    """

    found: dict[tuple[str, str], dict[str, str]] = {}
    cache: dict[str, tuple[str, list[int]]] = {}

    def syntax_of(path: str) -> tuple[str, list[int]]:
        if path not in cache:
            syntax = rust_syntax(sources[path])
            depth, running = [0] * (len(syntax) + 1), 0
            for index, character in enumerate(syntax):
                depth[index] = running
                running += (character == "{") - (character == "}")
            depth[len(syntax)] = running
            cache[path] = (syntax, depth)
        return cache[path]

    def visit(path: str, module: tuple[str, ...], module_dir: str, target: str, seen: set) -> None:
        path = _normal(path)
        if path not in sources or (path, module) in seen:
            return
        seen.add((path, module))
        syntax, _ = syntax_of(path)
        scan(path, 0, len(syntax), module, module_dir, str(PurePosixPath(path).parent), target, seen)

    def scan(path, start, end, module, module_dir, path_dir, target, seen) -> None:
        syntax, depth = syntax_of(path)
        text = sources[path]
        cursor = start
        while (match := ITEM.search(syntax, cursor, end)) is not None:
            cursor = match.end()
            if depth[match.start()] != depth[start]:
                continue
            if match.group(0) == "#[kani::proof]":
                function = PROOF_FUNCTION.match(syntax, match.end(), end)
                if function is not None:
                    name = function["name"]
                    found.setdefault((path, name), {})[target] = "::".join((*module, name))
                continue
            if match["module"] is None:
                close = _balanced_end(syntax, cursor - 1, "(", ")")
                argument = text[cursor:close - 1].strip() if close else ""
                if re.fullmatch(r'"[^"\\]+"', argument):
                    visit(f"{PurePosixPath(path).parent}/{argument[1:-1]}", module, module_dir, target, seen)
                cursor = close or cursor
                continue
            name = match["module"]
            explicit = PATH_ATTRIBUTE.search(text[match.start():match.end()])
            if match["delimiter"] == "{":
                close = _balanced_end(syntax, cursor - 1, "{", "}")
                if close is None:
                    continue
                child_dir = f"{path_dir}/{explicit.group(1)}" if explicit else f"{module_dir}/{name}"
                scan(path, cursor, close - 1, (*module, name), child_dir, child_dir, target, seen)
                cursor = close
            elif explicit:
                child = _normal(f"{path_dir}/{explicit.group(1)}")
                visit(child, (*module, name), _module_dir(child), target, seen)
            else:
                for child in (f"{module_dir}/{name}.rs", f"{module_dir}/{name}/mod.rs"):
                    visit(child, (*module, name), _module_dir(child), target, seen)

    for target, root in _targets(package_dir, manifest, sources).items():
        visit(root, (), str(PurePosixPath(root).parent), target, set())
    return found


def _declared_module_path(package_dir: str, relative: str, sources: dict[str, str]) -> tuple[str, ...] | None:
    """The module path of a file in the conventional layout, or None.

    ``src/a/b.rs`` (or ``src/a/b/mod.rs``) is ``a::b`` when every ancestor
    declares its child with a plain top-level ``mod child;``. Anything else --
    ``#[path]``, ``include!``, a declaration nested in an inline module, a
    binary target -- answers None and the caller walks the real module tree.
    """

    source_root = f"{package_dir}/src"
    pure = PurePosixPath(relative)
    try:
        parts = list(pure.relative_to(source_root).with_suffix("").parts)
    except ValueError:
        return None
    if parts == ["lib"]:
        return ()
    if parts[-1] == "mod":
        parts.pop()
    if not parts or parts[0] in {"lib", "main", "bin"}:
        return None
    for depth in range(len(parts)):
        name = parts[depth]
        parent = parts[:depth]
        candidates = (
            [f"{source_root}/lib.rs"] if not parent
            else [f"{source_root}/{'/'.join(parent)}.rs", f"{source_root}/{'/'.join(parent)}/mod.rs"]
        )
        parent_file = next((path for path in candidates if path in sources), None)
        declaration = re.compile(rf"^(?:pub(?:\([^)]*\))?\s+)?mod\s+{name}\s*;", re.MULTILINE)
        if parent_file is None or declaration.search(sources[parent_file]) is None:
            return None
        child = [f"{source_root}/{'/'.join(parts[:depth + 1])}.rs", f"{source_root}/{'/'.join(parts[:depth + 1])}/mod.rs"]
        if sum(path in sources for path in child) != 1:
            return None
    return tuple(parts)


def _inline_names(relative: str, text: str, module: tuple[str, ...]) -> dict[str, str] | None:
    """Harness name -> qualified name inside one file, or None if it holds a module edge."""

    syntax = rust_syntax(text)
    depth, running = [0] * (len(syntax) + 1), 0
    for index, character in enumerate(syntax):
        depth[index] = running
        running += (character == "{") - (character == "}")
    found: dict[str, str] = {}

    def scan(start: int, end: int, path: tuple[str, ...]) -> bool:
        cursor = start
        while (match := ITEM.search(syntax, cursor, end)) is not None:
            cursor = match.end()
            if depth[match.start()] != depth[start]:
                continue
            if match.group(0) == "#[kani::proof]":
                function = PROOF_FUNCTION.match(syntax, match.end(), end)
                if function is not None:
                    found[function["name"]] = "::".join((*path, function["name"]))
                continue
            if match["module"] is None or match["delimiter"] == ";" and path != module:
                return False
            if match["delimiter"] == ";":
                continue
            if PATH_ATTRIBUTE.search(text[match.start():match.end()]):
                return False
            close = _balanced_end(syntax, cursor - 1, "{", "}")
            if close is None or not scan(cursor, close - 1, (*path, match["module"])):
                return False
            cursor = close
        return True

    return found if scan(0, len(syntax), module) else None


# --------------------------------------------------------------------------
# The plan.


def harnesses(root: Path) -> list[Planned]:
    """Every harness with its selection, tier and qualified Kani names."""

    try:
        scan = formal_anchors.scan(root)
        manifests = source_text(root, formal_anchors.CARGO_MANIFESTS)
    except SourceError as error:
        raise PlanError(str(error)) from error
    errors = list(scan.errors)
    by_package: dict[str, list[formal_anchors.Harness]] = {}
    for harness in scan.harnesses:
        by_package.setdefault(harness.package or "", []).append(harness)
    directories: dict[str, str] = {}
    for relative, text in manifests.items():
        name = tomllib.loads(text).get("package", {}).get("name")
        if isinstance(name, str):
            directories[name] = str(PurePosixPath(relative).parent)
    planned: list[Planned] = []
    for package, members in sorted(by_package.items()):
        names: dict[tuple[str, str], dict[str, str]] = {}
        if package in directories:
            package_dir = directories[package]
            try:
                sources = source_text(root, f"{package_dir}/**/*.rs")
            except SourceError as error:
                raise PlanError(str(error)) from error
            manifest = tomllib.loads(manifests[f"{package_dir}/Cargo.toml"])
            # The conventional layout answers from file paths and one lexed file
            # per harness; any other shape walks the package's real module tree.
            library = "lib" in _targets(package_dir, manifest, sources)
            for relative in sorted({harness.path for harness in members}):
                module = _declared_module_path(package_dir, relative, sources) if library else None
                inline = _inline_names(relative, sources[relative], module) if module is not None else None
                if inline is None:
                    names = qualified_names(package_dir, manifest, sources)
                    break
                for name, qualified in inline.items():
                    names[(relative, name)] = {"lib": qualified}
        owner: dict[str, formal_anchors.Harness] = {}
        for harness in sorted(members, key=lambda item: (item.path, item.line)):
            selection, tier = _selection(harness, errors)
            targets = names.get((harness.path, harness.name), {})
            if selection.target == "lib":
                targets = {kind: name for kind, name in targets.items() if kind == "lib"}
            if not targets:
                errors.append(
                    f"{harness.where}: Kani harness {harness.name} is not reachable from "
                    f"{'the library' if selection.target == 'lib' else 'any'} Cargo target of "
                    f"{package or 'its package'}, so no filter can select it"
                )
                continue
            qualified = tuple(sorted(set(targets.values())))
            for name in qualified:
                if owner.setdefault(name, harness) is not harness:
                    errors.append(f"{harness.where}: Kani harness {name} is also defined at {owner[name].where}")
            planned.append(Planned(harness, selection, tier, qualified))
    if errors:
        raise PlanError("\n".join(sorted(set(errors))))
    return planned


def batches(root: Path, tier: str, affected: set[str] | None = None) -> list[Batch]:
    """The ``full`` tier runs every harness; ``pr`` leaves out the nightly ones.

    ``affected`` (package names, from ``affected_packages``) narrows the plan
    to the harnesses a change can alter; ``None`` keeps every package.
    """

    if tier not in ("pr", "full"):
        raise PlanError(f"unknown plan tier {tier!r}")
    grouped: dict[Selection, Batch] = {}
    for item in harnesses(root):
        if tier == "pr" and item.tier != "pr":
            continue
        if affected is not None and item.selection.package not in affected:
            continue
        grouped.setdefault(item.selection, Batch(item.selection)).names.extend(item.qualified)
    result = []
    for key in sorted(grouped):
        names = sorted(grouped[key].names)
        result.extend(
            Batch(key, names[start:start + MAX_HARNESSES_PER_BATCH])
            for start in range(0, len(names), MAX_HARNESSES_PER_BATCH)
        )
    return result


# A script this file names, or a Python module it imports from beside itself.
SCRIPT_REFERENCE = re.compile(r"scripts/ci/[A-Za-z0-9_][A-Za-z0-9_.\-]*(?:/[A-Za-z0-9_.\-]+)*")
PYTHON_IMPORT = re.compile(r"^\s*(?:from|import)\s+([A-Za-z_][A-Za-z0-9_]*)", re.MULTILINE)
# Resolve only the conventional binding to this shell entry's own directory.
# Other runtime directories are caller state and cannot name repository inputs.
SHELL_DIRECTORY_BINDING = 'script_dir="$(cd "$(dirname "$0")" && pwd)"'
SHELL_DIRECTORY_REFERENCE = re.compile(
    r"\$(?:script_dir|\{script_dir\})/([A-Za-z0-9_][A-Za-z0-9_.\-]*(?:/[A-Za-z0-9_][A-Za-z0-9_.\-]*)*)"
)
FORMAL_RUNNER = "scripts/ci/check_formal.sh"


def script_closure(root: Path, roots: tuple[str, ...]) -> set[str]:
    """Every repository file the given entry scripts reach, transitively.

    Consumers: ``affected_packages`` (a change to the formal runner's closure
    affects every harness) and the formal receipt fixture, which copies the
    closure instead of a hand-kept file list.
    """

    pending = list(roots)
    seen: set[str] = set()
    while pending:
        relative = pending.pop()
        if relative in seen or not (root / relative).is_file():
            continue
        seen.add(relative)
        if (root / relative).suffix in {".patch", ".c", ".rs", ".toml"}:
            continue
        text = (root / relative).read_text(encoding="utf-8", errors="ignore")
        references = SCRIPT_REFERENCE.findall(text)
        if relative.endswith(".sh") and SHELL_DIRECTORY_BINDING in text:
            parent = PurePosixPath(relative).parent
            references += [
                (parent / path).as_posix()
                for path in SHELL_DIRECTORY_REFERENCE.findall(text)
            ]
        if relative.endswith(".py"):
            parent = PurePosixPath(relative).parent
            references += [
                (parent / f"{module}.py").as_posix()
                for module in PYTHON_IMPORT.findall(text)
                if (root / parent / f"{module}.py").is_file()
            ]
        pending.extend(reference for reference in references if reference not in seen)
    return seen


def _dependencies(manifest: dict) -> set[str]:
    names: set[str] = set()
    tables = [manifest, *manifest.get("target", {}).values()]
    for table in tables:
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            for name, value in table.get(section, {}).items():
                names.add(value.get("package", name) if isinstance(value, dict) else name)
    return names


def affected_packages(root: Path, base: str) -> set[str] | None:
    """Packages whose harness results a change since ``base`` can alter.

    A changed file inside a package affects that package and, through Cargo,
    every workspace package that depends on it. A change to the formal
    runner's own closure, or to a file at the repository root (the workspace
    manifest, lock or toolchain), affects every harness: None. Anything else
    (documentation, the Web console, obligation files) runs no Kani batch.
    An unknown ``base`` is refused, never read as "nothing changed".
    """

    environment = repository_environment(root)
    if environment is None:
        raise PlanError(f"cannot list the changes since {base!r}: {root} is not a repository top")
    listed = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", base, "--"], cwd=root,
        capture_output=True, text=True, check=False, env=environment,
    )
    if listed.returncode != 0:
        raise PlanError(f"cannot list the changes since {base!r}: {listed.stderr.strip()}")
    untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard"], cwd=root,
        capture_output=True, text=True, check=True, env=environment,
    )
    changed = set(listed.stdout.split()) | set(untracked.stdout.split())
    runner = script_closure(root, (FORMAL_RUNNER,))
    manifests = {
        str(PurePosixPath(relative).parent): tomllib.loads(text)
        for relative, text in source_text(root, formal_anchors.CARGO_MANIFESTS).items()
    }
    names = {directory: manifest.get("package", {}).get("name") for directory, manifest in manifests.items()}
    touched: set[str] = set()
    for path in changed:
        if path in runner or "/" not in path:
            return None
        owner = max((directory for directory in names if path.startswith(f"{directory}/")), key=len, default=None)
        if owner is not None and names[owner]:
            touched.add(names[owner])
    dependents: dict[str, set[str]] = {}
    for directory, manifest in manifests.items():
        for dependency in _dependencies(manifest):
            dependents.setdefault(dependency, set()).add(names[directory])
    pending = list(touched)
    while pending:
        for dependent in dependents.get(pending.pop(), ()):
            if dependent not in touched:
                touched.add(dependent)
                pending.append(dependent)
    return touched


def digest(plan: list[Batch]) -> str:
    """Identity of a plan: the exact argv of every batch, in order."""

    encoded = json.dumps([batch.argv() for batch in plan], separators=(",", ":")).encode()
    return "sha256:" + hashlib.sha256(encoded).hexdigest()


def render(plan: list[Batch]) -> str:
    """One batch per line, `package argv...`, for the gate's `read -a` loop."""

    lines = []
    for batch in plan:
        argv = batch.argv()
        if any(not token or any(character.isspace() for character in token) for token in argv):
            raise PlanError(f"batch {batch.selection} has an argument the gate cannot split: {argv}")
        lines.append(" ".join(argv) + "\n")
    return "".join(lines)


def self_test() -> None:
    """Decision table for tags, names and batching.

    Causes: T1 no tag; T2 a profile tag; T3 features and target tags; T4 two
    harnesses share one selection; T5 a nightly tier tag; T6 a selection has
    exactly eight or more than eight harnesses; N1 an inline proof
    module, N2 a `mod x;` file, N3 a `#[path]` module, N4 an `include!`d file;
    R1 unknown key, R2 unknown profile, R3 a profile bound to another package,
    R4 a harness no Cargo target reaches, R5 two @kani lines, R6 malformed
    features or tier.
    Effects: E1 the default argv `package --exact --harness <qualified>`; E2 the
    profile's serialized argv; E3 cargo flags before the Kani options; E4 one
    batch; E5 present in `full`, absent from `pr`; E6 refusal naming file:line;
    E7 sorted chunks of at most eight, with every exact name once.
    Rules: T1 => E1; T2 => E2; T3 => E3; T4 => E4; T5 => E5; N1..N4 => the
    compiler's module path; R1..R6 => E6; T6 => E7.
    """

    import tempfile  # noqa: PLC0415 - only the self-test builds fixtures

    def harness(name: str, tag: str = "", indent: str = "") -> str:
        lines = [f"{indent}#[cfg(kani)]", f"{indent}/// @obligation run.safe"]
        lines += [f"{indent}/// @kani {tag}"] if tag else []
        return "\n".join([*lines, f"{indent}#[kani::proof]", f"{indent}fn {name}() {{}}", ""])

    def refused(root: Path, fragment: str) -> None:
        try:
            batches(root, "full")
        except PlanError as error:
            assert fragment in str(error), (fragment, str(error))
        else:
            raise AssertionError(f"planned a tree that must be refused: {fragment}")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        crate = root / "crates/a"
        (crate / "src/nested").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname = "a"\nversion = "0.0.0"\n')
        lib = (
            "mod file;\nmod nested;\n#[path = \"elsewhere.rs\"]\nmod renamed;\ninclude!(\"included.rs\");\n"
            + harness("root_default") + harness("root_kissat", "profile=solver-kissat-v1")
            + harness("root_features", "features=x,y target=lib default-features=false")
            + harness("root_slow", "tier=nightly")
            + "#[cfg(kani)]\nmod proofs {\n" + harness("inline_default", indent="    ") + "}\n"
        )
        (crate / "src/lib.rs").write_text(lib)
        (crate / "src/file.rs").write_text(harness("file_default"))
        (crate / "src/nested/mod.rs").write_text("mod deeper;\n")
        (crate / "src/nested/deeper.rs").write_text(harness("deeper_default"))
        (crate / "src/elsewhere.rs").write_text(harness("renamed_default"))
        (crate / "src/included.rs").write_text(harness("included_default"))
        plan = {tuple(batch.argv()) for batch in batches(root, "full")}
        qualified = sorted([
            "nested::deeper::deeper_default", "file::file_default", "included_default",
            "proofs::inline_default", "renamed::renamed_default", "root_default", "root_slow",
        ])
        expected = {
            ("a", "--exact", *(piece for name in qualified for piece in ("--harness", name))),
            ("a", "--solver", "kissat", "--exact", "--harness", "root_kissat"),
            ("a", "--lib", "--no-default-features", "--features", "x,y", "--exact", "--harness", "root_features"),
        }
        assert plan == expected, ("T1-T4 N1-N4", plan)
        pr = {tuple(batch.argv()) for batch in batches(root, "pr")}
        assert all("root_slow" not in argv for argv in pr) and len(pr) == 3, ("T5", pr)
        # T6 -> E7: seven full/six PR default proofs plus two reach the
        # boundary (PR 8) and cross it (full 9). A third crosses it in PR too.
        # The tier filter and chunk boundary must neither omit nor duplicate a
        # proof, and the original selection's Cargo options stay unchanged.
        for extra, expected_full, expected_pr in ((2, [8, 1], [8]), (3, [8, 2], [8, 1])):
            (crate / "src/lib.rs").write_text(
                lib + "".join(harness(f"extra_{number}") for number in range(extra))
            )
            for tier, sizes in (("full", expected_full), ("pr", expected_pr)):
                selected = [
                    batch for batch in batches(root, tier)
                    if batch.selection == Selection("a")
                ]
                assert [len(batch.names) for batch in selected] == sizes, (tier, extra, selected)
                names = [name for batch in selected for name in batch.names]
                expected_names = sorted(
                    name for name in [*qualified, *(f"extra_{number}" for number in range(extra))]
                    if tier == "full" or name != "root_slow"
                )
                assert names == expected_names, (tier, extra, names)
                assert all(batch.argv()[1] == "--exact" for batch in selected)
        (crate / "src/lib.rs").write_text(lib)
        for tag, fragment in (
            ("colour=red", "crates/a/src/file.rs:3: @kani has unknown key 'colour'"),
            ("profile=missing-v1", "unknown Kani profile 'missing-v1'"),
            ("profile=fingerprint-fnv-hex-v1", "is bound to package awaken-agent-contract, not a"),
            ("features=x,x", "features must be distinct"),
            ("tier=weekly", "tier must be one of pr, nightly"),
        ):
            (crate / "src/file.rs").write_text(harness("file_default", tag))
            refused(root, fragment)
        (crate / "src/file.rs").write_text(harness("file_default", "tier=pr").replace(
            "/// @kani tier=pr", "/// @kani tier=pr\n/// @kani tier=pr"))
        refused(root, "a harness carries at most one @kani line (file_default)")
        (crate / "src/file.rs").write_text(harness("file_default"))
        (crate / "src/orphan.rs").write_text(harness("orphan_default"))
        refused(root, "crates/a/src/orphan.rs:4: Kani harness orphan_default is not reachable")

    # D1/D2 literal and braced script_dir references follow the entry's own
    # directory; D3 unknown variables, D4 unbound script_dir, D5 parent escapes
    # and D6 missing files add no inputs. D7 a newly referenced nested input
    # enters the closure without editing a second dependency list.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        scripts = root / "scripts/ci"
        (scripts / "fixtures/nested").mkdir(parents=True)
        entry = scripts / "probe.sh"
        first = scripts / "fixtures/first.toml"
        second = scripts / "fixtures/nested/second.rs"
        third = scripts / "fixtures/nested/third.lock"
        external = root / "escape.rs"
        for path in (first, second, third, external):
            path.write_text("fixture\n")
        references = (
            'sha256sum "$script_dir/fixtures/first.toml" '
            '"${script_dir}/fixtures/nested/second.rs" '
            '"$unknown/fixtures/nested/third.lock" '
            '"$script_dir/../../escape.rs" "$script_dir/fixtures/missing.rs"\n'
        )
        entry.write_text(SHELL_DIRECTORY_BINDING + "\n" + references)
        expected = {"scripts/ci/probe.sh", "scripts/ci/fixtures/first.toml",
                    "scripts/ci/fixtures/nested/second.rs"}
        assert script_closure(root, ("scripts/ci/probe.sh",)) == expected, "D1-D3 D5-D6"
        entry.write_text(references)
        assert script_closure(root, ("scripts/ci/probe.sh",)) == {"scripts/ci/probe.sh"}, "D4"
        entry.write_text(SHELL_DIRECTORY_BINDING + "\n" + references +
                         'sha256sum "$script_dir/fixtures/nested/third.lock"\n')
        assert script_closure(root, ("scripts/ci/probe.sh",)) == expected | {
            "scripts/ci/fixtures/nested/third.lock"}, "D7"

    # Affected packages. A1 a change in b affects b and its dependent a, not
    # c; A2 a documentation change affects no package; A3 a root file (the
    # lock) and A4 the formal runner affect everything (None); A5 an unknown
    # base is refused, never read as "nothing changed".
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        for name, dependencies in (("a", '[dependencies]\nb = { path = "../b" }\n'), ("b", ""), ("c", "")):
            (root / f"crates/{name}/src").mkdir(parents=True)
            (root / f"crates/{name}/Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "0.0.0"\n{dependencies}')
            (root / f"crates/{name}/src/lib.rs").write_text(harness(f"{name}_proof"))
        (root / "docs").mkdir()
        (root / "docs/x.md").write_text("x\n")
        (root / "Cargo.lock").write_text("# lock\n")
        (root / "scripts/ci").mkdir(parents=True)
        (root / FORMAL_RUNNER).write_text("python3 scripts/ci/formal_plan.py kani --tier full\n")
        git = ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "core.hooksPath=/dev/null"]
        detached = detached_git_environment()
        for arguments in (["init", "--quiet"], ["add", "."], ["commit", "--quiet", "-m", "fixture"]):
            subprocess.run([*git, *arguments], cwd=root, check=True, capture_output=True, env=detached)
        for edited, expected in (
            ("crates/b/src/lib.rs", {"a", "b"}), ("docs/x.md", set()),
            ("Cargo.lock", None), (FORMAL_RUNNER, None),
        ):
            original = (root / edited).read_text()
            (root / edited).write_text(original + "// changed\n")
            assert affected_packages(root, "HEAD") == expected, (edited, affected_packages(root, "HEAD"))
            (root / edited).write_text(original)
        (root / "crates/b/src/lib.rs").write_text(harness("b_proof") + "// changed\n")
        planned = {tuple(batch.argv()) for batch in batches(root, "pr", affected_packages(root, "HEAD"))}
        assert planned == {("a", "--exact", "--harness", "a_proof"), ("b", "--exact", "--harness", "b_proof")}, planned
        try:
            affected_packages(root, "no-such-base")
        except PlanError as error:
            assert "cannot list the changes since 'no-such-base'" in str(error), "A5"
        else:
            raise AssertionError("A5 an unknown base must be refused")
    print("formal_plan self-test: the decision table holds")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("plan", nargs="?", choices=("kani",), help="the plan to print (one batch per line)")
    parser.add_argument("--tier", choices=("pr", "full"), help="pr leaves out tier=nightly harnesses")
    parser.add_argument("--affected", metavar="BASE", help="only the harnesses a change since BASE can alter")
    parser.add_argument("--digest", action="store_true", help="print the plan digest instead of the batches")
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        if arguments.plan or arguments.tier or arguments.digest or arguments.affected:
            parser.error("--self-test takes no plan arguments")
        self_test()
        return 0
    if arguments.plan != "kani" or arguments.tier is None:
        parser.error("print a plan with `kani --tier pr|full [--affected BASE]`, or run --self-test")
    try:
        affected = affected_packages(ROOT, arguments.affected) if arguments.affected else None
        plan = batches(ROOT, arguments.tier, affected)
        sys.stdout.write(f"{digest(plan)}\n" if arguments.digest else render(plan))
    except (PlanError, ValueError) as error:
        print(f"formal plan: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
