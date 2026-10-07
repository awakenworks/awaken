"""Workspace discovery helpers for the crate-boundary fitness check."""

from __future__ import annotations

import re
import tomllib
from collections.abc import Callable, Iterable
from functools import lru_cache
from pathlib import Path, PurePath, PurePosixPath, PureWindowsPath

import _arch_fitness
import _crate_dependency_fitness


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"
_RUST_PROJECTION_CACHE_SIZE = 2048


def repository_path(path: PurePath, repo_root: PurePath) -> str:
    """Stable repository identity for source inventories and ownership keys."""
    return path.relative_to(repo_root).as_posix()


def load_manifest(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def package_name(manifest: dict) -> str:
    return str(manifest["package"]["name"])


_NOT_CRATE_PARTS = frozenset({"target", "node_modules"})


def crate_manifests(crates: Path = CRATES) -> list[Path]:
    """Every crate manifest under `crates`, at whatever directory depth.

    Discovery never assumes `crates/<group>/<crate>`: a crate is any directory
    holding a `Cargo.toml`, except a manifest nested inside another crate (a
    test fixture), so moving a crate to a deeper directory cannot silently drop
    it from a gate.
    """
    if not crates.exists():
        return []
    found = [
        path
        for path in crates.rglob("Cargo.toml")
        if not _NOT_CRATE_PARTS.intersection(path.relative_to(crates).parts)
    ]
    roots = {path.parent for path in found}
    return sorted(
        path for path in found if not any(parent in roots for parent in path.parent.parents)
    )


def crate_manifest(crate_name: str, crates: Path = CRATES) -> Path | None:
    """The manifest of the crate whose directory is named `crate_name`."""
    return next((path for path in crate_manifests(crates) if path.parent.name == crate_name), None)


def crate_source_files(crates: Path = CRATES, tree: str = "src") -> list[Path]:
    """Every `.rs` file under the `tree` directory of every crate."""
    return sorted(
        path
        for manifest in crate_manifests(crates)
        for path in (manifest.parent / tree).rglob("*.rs")
    )


def ensure_fixture_manifest(source: Path) -> None:
    """Give a self-test fixture source the `Cargo.toml` a real crate has.

    The crate root is the directory holding the path's first `src`/`tests`
    component, so a decision table exercises the same discovery as the tree.
    """
    parts = source.parts
    index = next(i for i, part in enumerate(parts) if part in {"src", "tests"})
    manifest = Path(*parts[:index]) / "Cargo.toml"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    if not manifest.exists():
        manifest.write_text(f'[package]\nname = "{parts[index - 1]}"\n', encoding="utf-8")


def workspace_member_dirs(repo_root: Path = REPO_ROOT) -> list[Path]:
    """The crate directories the root `[workspace]` declares, globs expanded."""
    workspace = load_manifest(repo_root / "Cargo.toml").get("workspace", {})
    excluded = {(repo_root / pattern).resolve() for pattern in workspace.get("exclude", [])}
    members: set[Path] = set()
    for pattern in workspace.get("members", []):
        for path in repo_root.glob(pattern):
            if (path / "Cargo.toml").is_file() and path.resolve() not in excluded:
                members.add(path.resolve())
    return sorted(members)


def check_crate_manifests_match_workspace(repo_root: Path = REPO_ROOT) -> list[str]:
    """Gate discovery and the Cargo workspace name the same crate set."""
    root = repo_root.resolve()
    discovered = {path.parent.resolve() for path in crate_manifests(repo_root / "crates")}
    members = set(workspace_member_dirs(repo_root))
    errors = [
        f"{repository_path(path, root)}: crate under crates/ is not a workspace member"
        for path in sorted(discovered - members)
    ]
    errors += [
        f"{repository_path(path, root)}: workspace member is not discovered under crates/"
        for path in sorted(members - discovered)
    ]
    return errors


def iter_crate_manifests() -> list[Path]:
    return crate_manifests(CRATES)


def text_files(crate_name: str) -> list[Path]:
    manifest = crate_manifest(crate_name)
    if manifest is None:
        return []
    src = manifest.parent / "src"
    if not src.exists():
        return []
    return sorted(path for path in src.rglob("*.rs") if path.is_file())


def _blank_preserving_lines(source: str) -> str:
    return "".join(character if character in "\r\n" else " " for character in source)


def _raw_string_end(source: str, start: int) -> int | None:
    if start > 0 and (source[start - 1].isalnum() or source[start - 1] == "_"):
        return None
    for prefix in ("br", "cr", "r"):
        if not source.startswith(prefix, start):
            continue
        cursor = start + len(prefix)
        while cursor < len(source) and source[cursor] == "#":
            cursor += 1
        if cursor >= len(source) or source[cursor] != '"':
            continue
        delimiter = '"' + source[start + len(prefix) : cursor]
        end = source.find(delimiter, cursor + 1)
        return len(source) if end < 0 else end + len(delimiter)
    return None


def _quoted_string_end(source: str, start: int) -> int:
    cursor = start + 1
    while cursor < len(source):
        if source[cursor] == "\\":
            cursor += 2
        elif source[cursor] == '"':
            return cursor + 1
        else:
            cursor += 1
    return len(source)


def _character_literal_end(source: str, start: int) -> int | None:
    if start + 2 < len(source) and source[start + 2] == "'":
        return start + 3
    if start + 1 >= len(source) or source[start + 1] != "\\":
        return None
    cursor = start + 1
    escaped = False
    while cursor < len(source):
        if source[cursor] in "\r\n":
            return None
        if escaped:
            escaped = False
        elif source[cursor] == "\\":
            escaped = True
        elif source[cursor] == "'":
            return cursor + 1
        cursor += 1
    return None


# Every lexer decision below starts at one of these characters; any other run
# of characters is copied unchanged, so the loops jump over it in one step.
_COMMENT_LEXEME_START = re.compile(r"[bcr\"'/]")
_LITERAL_LEXEME_START = re.compile(r"[bcr\"']")


@lru_cache(maxsize=_RUST_PROJECTION_CACHE_SIZE)
def rust_without_comments(source: str) -> str:
    """Blank Rust comments without changing offsets, lines, or string literals."""
    output: list[str] = []
    cursor = 0
    while cursor < len(source):
        start = _COMMENT_LEXEME_START.search(source, cursor)
        if start is None:
            output.append(source[cursor:])
            break
        if start.start() > cursor:
            output.append(source[cursor:start.start()])
            cursor = start.start()
        raw_end = _raw_string_end(source, cursor)
        if raw_end is not None:
            output.append(source[cursor:raw_end])
            cursor = raw_end
            continue
        if source[cursor] == '"':
            string_end = _quoted_string_end(source, cursor)
            output.append(source[cursor:string_end])
            cursor = string_end
            continue
        if source[cursor] == "'":
            character_end = _character_literal_end(source, cursor)
            if character_end is not None:
                output.append(source[cursor:character_end])
                cursor = character_end
                continue
        if source.startswith("//", cursor):
            end = source.find("\n", cursor + 2)
            end = len(source) if end < 0 else end
            output.append(_blank_preserving_lines(source[cursor:end]))
            cursor = end
            continue
        if source.startswith("/*", cursor):
            depth = 1
            end = cursor + 2
            while end < len(source) and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            output.append(_blank_preserving_lines(source[cursor:end]))
            cursor = end
            continue
        output.append(source[cursor])
        cursor += 1
    return "".join(output)


@lru_cache(maxsize=_RUST_PROJECTION_CACHE_SIZE)
def rust_syntax(source: str) -> str:
    """Blank comments and literals while retaining Rust syntax and line structure."""
    source = rust_without_comments(source)
    output: list[str] = []
    cursor = 0
    while cursor < len(source):
        start = _LITERAL_LEXEME_START.search(source, cursor)
        if start is None:
            output.append(source[cursor:])
            break
        if start.start() > cursor:
            output.append(source[cursor:start.start()])
            cursor = start.start()
        raw_end = _raw_string_end(source, cursor)
        if raw_end is not None:
            output.append(_blank_preserving_lines(source[cursor:raw_end]))
            cursor = raw_end
            continue
        if source[cursor] == '"':
            string_end = _quoted_string_end(source, cursor)
            output.append(_blank_preserving_lines(source[cursor:string_end]))
            cursor = string_end
            continue
        if source[cursor] == "'":
            character_end = _character_literal_end(source, cursor)
            if character_end is not None:
                output.append(_blank_preserving_lines(source[cursor:character_end]))
                cursor = character_end
                continue
        output.append(source[cursor])
        cursor += 1
    return "".join(output)


def _balanced_end(source: str, start: int, opening: str, closing: str) -> int | None:
    depth = 0
    for cursor in range(start, len(source)):
        if source[cursor] == opening:
            depth += 1
        elif source[cursor] == closing:
            depth -= 1
            if depth == 0:
                return cursor + 1
    return None


def _top_level_arguments(source: str) -> list[str]:
    arguments: list[str] = []
    start = 0
    depth = 0
    for cursor, character in enumerate(source):
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif character == "," and depth == 0:
            arguments.append(source[start:cursor])
            start = cursor + 1
    arguments.append(source[start:])
    return arguments


def _cfg_value_when_not_test(predicate: str) -> bool | None:
    predicate = predicate.strip()
    # `kani` is set only by the Kani compiler, so a production build sees it as
    # false exactly as it sees `test`. Without this atom a `#[cfg(kani)] mod
    # proofs;` kept its proof file in the production module graph, and every
    # gate built on this projection counted and scanned verification code as
    # shipped code.
    if predicate in {"test", "kani"}:
        return False
    operation = re.match(r"^(all|any|not)\s*\(", predicate)
    if operation is None:
        return None
    opening = predicate.find("(", operation.start())
    closing = _balanced_end(predicate, opening, "(", ")")
    if closing is None or predicate[closing:].strip():
        return None
    values = [
        _cfg_value_when_not_test(argument)
        for argument in _top_level_arguments(predicate[opening + 1 : closing - 1])
        if argument.strip()
    ]
    if operation.group(1) == "not":
        if len(values) != 1 or values[0] is None:
            return None
        return not values[0]
    if operation.group(1) == "all":
        if any(value is False for value in values):
            return False
        return True if all(value is True for value in values) else None
    if any(value is True for value in values):
        return True
    return False if all(value is False for value in values) else None


def _cfg_attribute_is_test_only(attribute: str) -> bool:
    body = attribute[attribute.find("[") + 1 : attribute.rfind("]")]
    cfg = re.match(r"^\s*cfg\s*\((?P<predicate>.*)\)\s*$", body, re.DOTALL)
    if cfg is None or re.search(r"\b(?:test|kani)\b", cfg.group("predicate")) is None:
        return False
    return _cfg_value_when_not_test(cfg.group("predicate")) is False


def _outer_attribute_end(source: str, start: int) -> int | None:
    if source[start] != "#":
        return None
    cursor = start + 1
    while cursor < len(source) and source[cursor].isspace():
        cursor += 1
    if cursor >= len(source) or source[cursor] != "[":
        return None
    return _balanced_end(source, cursor, "[", "]")


def _first_expression_brace(source: str, start: int) -> int | None:
    parenthesis_depth = 0
    bracket_depth = 0
    for cursor in range(start, len(source)):
        character = source[cursor]
        if character == "(":
            parenthesis_depth += 1
        elif character == ")":
            parenthesis_depth = max(0, parenthesis_depth - 1)
        elif character == "[":
            bracket_depth += 1
        elif character == "]":
            bracket_depth = max(0, bracket_depth - 1)
        elif character == "{" and parenthesis_depth == 0 and bracket_depth == 0:
            return cursor
        elif character == ";" and parenthesis_depth == 0 and bracket_depth == 0:
            return None
    return None


def _if_expression_end(source: str, start: int) -> int | None:
    opening = _first_expression_brace(source, start)
    if opening is None:
        return None
    end = _balanced_end(source, opening, "{", "}")
    if end is None:
        return None
    while True:
        cursor = end
        while cursor < len(source) and source[cursor].isspace():
            cursor += 1
        alternative = re.match(r"else\b", source[cursor:])
        if alternative is None:
            return end
        cursor += alternative.end()
        while cursor < len(source) and source[cursor].isspace():
            cursor += 1
        if re.match(r"if\b", source[cursor:]) is not None:
            opening = _first_expression_brace(source, cursor)
        elif cursor < len(source) and source[cursor] == "{":
            opening = cursor
        else:
            return None
        if opening is None:
            return None
        end = _balanced_end(source, opening, "{", "}")
        if end is None:
            return None


def _rust_item_end(source: str, start: int) -> int | None:
    cursor = start
    while cursor < len(source) and source[cursor].isspace():
        cursor += 1
    if cursor == len(source):
        return None
    declaration = source[cursor:]
    if declaration.startswith("{"):
        return _balanced_end(source, cursor, "{", "}")
    if re.match(r"if\b", declaration) is not None:
        return _if_expression_end(source, cursor)
    if re.match(
        r"(?:(?:async\s+(?:move\s+)?)|(?:const|unsafe|try)\s*)\{|"
        r"(?:match|while|for|loop)\b",
        declaration,
    ) is not None:
        opening = _first_expression_brace(source, cursor)
        return (
            None
            if opening is None
            else _balanced_end(source, opening, "{", "}")
        )
    owns_body = re.match(
        r"^(?:pub(?:\s*\([^)]*\))?\s+)?"
        r"(?:(?:unsafe|async|const|default)\s+)*"
        r"(?:(?:extern(?:\s+\"[^\"]*\")?)\s+)?"
        r"(?:fn|mod|impl|trait|struct|enum|union)\b|"
        r"^(?:pub(?:\s*\([^)]*\))?\s+)?(?:unsafe\s+)?extern\s*\{|"
        r"^macro_rules\s*!",
        declaration,
    ) is not None
    ends_with_semicolon = re.match(
        r"^(?:pub(?:\s*\([^)]*\))?\s+)?"
        r"(?:use|type|const|static|let|extern\s+crate)\b",
        declaration,
    ) is not None
    parenthesis_depth = 0
    bracket_depth = 0
    brace_depth = 0
    angle_depth = 0
    while cursor < len(source):
        character = source[cursor]
        if character == "(":
            parenthesis_depth += 1
        elif character == ")":
            parenthesis_depth = max(0, parenthesis_depth - 1)
        elif character == "[":
            bracket_depth += 1
        elif character == "]":
            bracket_depth = max(0, bracket_depth - 1)
        elif character == "<":
            angle_depth += 1
        elif character == ">":
            angle_depth = max(0, angle_depth - 1)
        elif character == "{" and not owns_body:
            brace_depth += 1
        elif character == "}" and not owns_body:
            brace_depth = max(0, brace_depth - 1)
        elif parenthesis_depth == 0 and bracket_depth == 0 and brace_depth == 0:
            # Semicolon-owned items cannot contain a top-level semicolon inside
            # generic arguments. Do not let expression `<`/`<<` tokens turn
            # comparison or shift syntax into an unterminated generic depth.
            if character == ";":
                return cursor + 1
            if character == "," and not owns_body and not ends_with_semicolon:
                if angle_depth == 0:
                    return cursor + 1
            if character == "{" and owns_body and angle_depth == 0:
                end = _balanced_end(source, cursor, "{", "}")
                if end is None:
                    return None
                return end + 1 if end < len(source) and source[end] == ";" else end
        cursor += 1
    return None


def test_only_rust_ranges(source: str) -> list[tuple[int, int]]:
    """Return source ranges provably disabled whenever ``test`` is false."""
    syntax = rust_syntax(source)
    ranges: list[tuple[int, int]] = []
    cursor = 0
    while cursor < len(syntax):
        attribute_start = syntax.find("#", cursor)
        if attribute_start < 0:
            break
        attribute_end = _outer_attribute_end(syntax, attribute_start)
        if attribute_end is None:
            cursor = attribute_start + 1
            continue
        attributes: list[str] = []
        item_start = attribute_start
        next_attribute = attribute_start
        while True:
            end = _outer_attribute_end(syntax, next_attribute)
            if end is None:
                break
            attributes.append(syntax[next_attribute:end])
            next_attribute = end
            while next_attribute < len(syntax) and syntax[next_attribute].isspace():
                next_attribute += 1
            if next_attribute >= len(syntax) or syntax[next_attribute] != "#":
                break
        if not any(_cfg_attribute_is_test_only(attribute) for attribute in attributes):
            cursor = attribute_end
            continue
        item_end = _rust_item_end(syntax, next_attribute)
        if item_end is None:
            cursor = attribute_end
            continue
        ranges.append((item_start, item_end))
        cursor = item_end
    return ranges


@lru_cache(maxsize=_RUST_PROJECTION_CACHE_SIZE)
def production_rust(source: str) -> str:
    """Project Rust source compiled with ``cfg(test) == false``.

    Only items whose cfg predicate is provably test-only are blanked. Compound
    alternatives such as ``cfg(any(test, feature = ...))`` remain production
    candidates, and removing one test item never hides a later production item.
    Comments are blanked after projection so source gates inspect executable
    vocabulary while preserving line numbers and character offsets.
    """
    projected = list(source)
    for start, end in test_only_rust_ranges(source):
        projected[start:end] = _blank_preserving_lines(source[start:end])
    return rust_without_comments("".join(projected))


def compiled_module_files(manifests: Iterable[Path]) -> set[Path]:
    """Every module some build compiles: production, `cfg(test)` or `cfg(kani)`.

    The production graph answers "is this shipped?". A proof or test module is
    not shipped, yet it is compiled -- by the test harness or by Kani -- so it
    is not dead code either. Evidence registered for an obligation must be
    attached to *some* build, which is this wider question.
    """
    return production_module_files(manifests, project=rust_without_comments)


def production_module_files(
    manifests: Iterable[Path],
    *,
    project: Callable[[str], str] = production_rust,
    include_test_targets: bool = False,
) -> set[Path]:
    """Structural module reachability from Cargo library/binary roots.

    With `include_test_targets`, integration-test and bench crate roots
    (`tests/*.rs`, `tests/*/main.rs`, `benches/...` and explicit `[[test]]`/
    `[[bench]]` paths) are roots too. A build script is never a test target:
    `cargo test` does not run tests compiled into `build.rs`.

    Reuses the production lexer: test-only branches do not connect evidence,
    while feature/platform branches are a conservative union, not proof that a
    particular release configuration executes them. Compiler checks still own
    cfg resolution, macro expansion and generated OUT_DIR code.
    """
    reachable: set[Path] = set()
    visited: set[tuple[Path, Path]] = set()

    def visit(path: Path, module_dir: Path | None = None) -> None:
        path = path.resolve()
        if not path.is_file():
            return
        if module_dir is None:
            module_dir = path.parent if path.name in {"lib.rs", "main.rs", "mod.rs"} else path.with_suffix("")
        key = (path, module_dir)
        if key in visited:
            return
        visited.add(key)
        reachable.add(path)
        source = project(path.read_text(encoding="utf-8"))
        scan(source, path, module_dir, path.parent)

    def scan(source: str, path: Path, module_dir: Path, path_dir: Path) -> None:
        syntax = rust_syntax(source)
        declaration = re.compile(
            r"(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?"
            r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*([;{])"
            r"|\binclude!\s*\("
        )
        cursor = 0
        while match := declaration.search(syntax, cursor):
            cursor = match.end()
            if match.group(1) is None:
                end = _balanced_end(syntax, cursor - 1, "(", ")")
                if end is None:
                    continue
                argument = source[cursor:end - 1].strip()
                if re.fullmatch(r'"[^"\\]+"', argument):
                    visit(path.parent / argument[1:-1], module_dir)
                cursor = end
                continue
            name, delimiter = match.group(1, 2)
            attributes = source[match.start():match.end()]
            explicit = re.search(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]', attributes)
            if delimiter == "{":
                end = _balanced_end(syntax, cursor - 1, "{", "}")
                if end is not None:
                    child_dir = path_dir / explicit.group(1) if explicit else module_dir / name
                    scan(source[cursor:end - 1], path, child_dir, child_dir)
                    cursor = end
            elif explicit:
                visit(path_dir / explicit.group(1))
            else:
                for child in (module_dir / (name + ".rs"), module_dir / name / "mod.rs"):
                    visit(child)

    for manifest_path in manifests:
        manifest = load_manifest(manifest_path)
        root = manifest_path.parent
        package = manifest.get("package", {})
        library = manifest.get("lib", {})
        if "lib" in manifest or (package.get("autolib", True) and (root / "src/lib.rs").is_file()):
            library_path = root / library.get("path", "src/lib.rs")
            visit(library_path, library_path.parent)
        for binary in manifest.get("bin", []):
            if "path" in binary:
                binary_path = root / binary["path"]
                visit(binary_path, binary_path.parent)
            elif name := binary.get("name"):
                paths = [root / "src/bin" / (name + ".rs"), root / "src/bin" / name / "main.rs"]
                if name == package.get("name"):
                    paths.append(root / "src/main.rs")
                for binary_path in paths:
                    visit(binary_path, binary_path.parent)
        if package.get("autobins", True):
            visit(root / "src/main.rs", root / "src")
            for binary_path in (root / "src/bin").glob("*.rs"):
                visit(binary_path, binary_path.parent)
            for binary_path in (root / "src/bin").glob("*/main.rs"):
                visit(binary_path, binary_path.parent)
        if include_test_targets:
            for kind, directory in (("test", "tests"), ("bench", "benches")):
                for target in manifest.get(kind, []):
                    if "path" in target:
                        target_path = root / target["path"]
                        visit(target_path, target_path.parent)
                for target_path in (root / directory).glob("*.rs"):
                    visit(target_path, target_path.parent)
                for target_path in (root / directory).glob("*/main.rs"):
                    visit(target_path, target_path.parent)
    return reachable


def normal_dependency_names(manifest: dict) -> set[str]:
    """Normal + build deps only; tests/examples are composition roots."""
    deps: set[str] = set()
    for section in ("dependencies", "build-dependencies"):
        deps.update(manifest.get(section, {}).keys())
    return deps


def public_first_party_reexports(sources: Iterable[str]) -> frozenset[str]:
    return frozenset(
        owner
        for source in sources
        for owner in re.findall(r"(?m)^\s*pub\s+use\s+(awaken_[A-Za-z0-9_]+)", source)
    )


def _selftest_crate_discovery() -> None:
    # Discovery cause/effect table. Causes: C1 a crate two levels below
    # crates/; C2 a crate three levels below; C3 a Cargo.toml nested inside a
    # crate (fixture); C4 a Cargo.toml under target/; C5 a workspace member
    # missing from crates/ or a crate missing from the members.
    # Effects: E1 discovered; E2 not discovered; E3 consistency error.
    # | Rule | cause | effect |
    # | D1   | C1    | E1     |
    # | D2   | C2    | E1     |
    # | D3   | C3    | E2     |
    # | D4   | C4    | E2     |
    # | D5   | C5    | E3     |
    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        crates = root / "crates"
        for relative in (
            "group/shallow",
            "product/context/deep",
            "group/shallow/tests/fixture",
            "group/shallow/target/debug/build",
        ):
            (crates / relative / "src").mkdir(parents=True)
            (crates / relative / "Cargo.toml").write_text("[package]\nname = 'x'\n")
            (crates / relative / "src" / "lib.rs").write_text("")
        names = [path.parent.name for path in crate_manifests(crates)]
        assert names == ["shallow", "deep"] or names == ["deep", "shallow"], names
        assert "shallow" in names, "D1"
        assert "deep" in names, "D2"
        assert "fixture" not in names, "D3"
        assert "build" not in names, "D4"
        assert crate_manifest("deep", crates) == crates / "product/context/deep/Cargo.toml", "D2"
        assert len(crate_source_files(crates)) == 2, "D1/D2 sources; D3/D4 excluded"
        (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/group/*"]\n')
        errors = check_crate_manifests_match_workspace(root)
        assert errors == ["crates/product/context/deep: crate under crates/ is not a workspace member"], errors
        (root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/group/*", "crates/product/*/*", "extra/*"]\n'
        )
        (root / "extra" / "outside").mkdir(parents=True)
        (root / "extra" / "outside" / "Cargo.toml").write_text("[package]\nname = 'y'\n")
        errors = check_crate_manifests_match_workspace(root)
        assert errors == ["extra/outside: workspace member is not discovered under crates/"], errors


def _selftest_projection_cache() -> None:
    # Cache cause/effect graph: C1 identical source is projected repeatedly;
    # C2 source changes at the same path/length; C3 more than the bound's unique
    # contents are examined. E1 reuse exactly the immutable result; E2 changed
    # syntax remains visible; E3 retained cache size stays bounded. Rules
    # K1=C1=>E1, K2=C2=>E2, K3=C3=>E3. Source CONTENT, not mtime/path, is the key;
    # never cache mutable range lists or filesystem discovery.
    for projector in (rust_without_comments, rust_syntax, production_rust):
        projector.cache_clear()
        original = 'fn live() { let marker = 1; } // keep offsets\n'
        changed = original.replace('marker', 'latest')
        first = projector(original)
        assert projector(original) is first, "K1 exact immutable projection reuse"
        assert projector.cache_info().hits == 1, "K1 only one lexical computation"
        assert 'latest' in projector(changed) and 'marker' not in projector(changed), "K2 no stale syntax"
        for item in range(_RUST_PROJECTION_CACHE_SIZE + 2):
            projector(f'fn retained_{item}() {{}}')
        assert projector.cache_info().currsize == _RUST_PROJECTION_CACHE_SIZE, "K3 bounded contents"
        assert projector(original) == first, "K3 eviction preserves semantics"
        projector.cache_clear()


def selftest() -> None:
    """Cause/effect table for first-party facade discovery.

    This module is a library, not an entry point: `check_crate_boundaries.py`
    runs this table before it judges the tree, so `python3
    _crate_boundary_workspace.py --self-test` is not a command.

    Causes: C1 crate-root source; C2 nested-module source; C3 private import.
    Effects: E1 public first-party owner is reported; E2 private use is ignored.

    | Rule | root public | nested public | private | effect |
    | R1   | yes         | no            | no      | E1     |
    | R2   | no          | yes           | no      | E1     |
    | R3   | no          | no            | yes     | E2     |
    """
    # P1/P2 identical POSIX/Windows subjects match the same ownership key;
    # P3 different subjects stay distinct; P4 an outside path is never admitted.
    expected = "crates/server/example/src/lib.rs"
    for root in (PurePosixPath("/repo"), PureWindowsPath("C:/repo")):
        assert repository_path(root / expected, root) == expected, "P1/P2"
        assert repository_path(root / "crates/server/other/src/lib.rs", root) != expected, "P3"
        try:
            repository_path(root.parent / "outside/src/lib.rs", root)
        except ValueError:
            pass
        else:
            raise AssertionError("P4 outside source must not become an ownership key")
    _selftest_projection_cache()
    _selftest_crate_discovery()
    # Production-source cause/effect table: C4 an exact/`all` cfg predicate is
    # false whenever test=false; C5 `not(test)` or `any(test, feature=...)` can
    # compile outside tests; C6 a test-only item precedes later production; C7
    # comments contain fake code while string literals contain comment tokens;
    # C8 cfg(test) owns a semicolon-free if/block expression before a cfg(not
    # (test)) or ordinary sibling. E7 removes only C8 and preserves the sibling.
    # Effects: E4 blank only C4 items; E5 preserve C5/C6; E6 blank comments
    # without changing lines or literals. These rules make this helper the one
    # production-Rust authority used by every crate-boundary fitness gate.
    assert "fixture" not in production_rust(
        '#[cfg(test)]\nmod tests { const fixture: &str = "fixture"; }'
    ), "R4/E4 exact test module"
    assert "fixture" not in production_rust(
        '#[cfg(all(feature = "loom", test))]\nconst fixture: usize = 1;'
    ), "R4/E4 compound test-only item"
    for retained in (
        '#[cfg(not(test))]\nconst PRODUCTION: usize = 1;',
        '#[cfg(any(test, feature = "support"))]\nconst PRODUCTION: usize = 1;',
    ):
        assert "PRODUCTION" in production_rust(retained), "R5/E5"
    # R9 `kani` is false in every production build, exactly like `test`: an item
    # or module gated on kani alone, or on any mix of test and kani, is
    # verification-only (E4); negating it or pairing it with a feature can
    # compile in production (E5).
    for verification in (
        "#[cfg(kani)]\nmod proofs { const fixture: usize = 1; }",
        "#[cfg(any(test, kani))]\nconst fixture: usize = 1;",
        "#[cfg(all(kani, feature = \"deep\"))]\nconst fixture: usize = 1;",
    ):
        assert "fixture" not in production_rust(verification), f"R9/E4 {verification!r}"
    for retained in (
        "#[cfg(not(kani))]\nconst PRODUCTION: usize = 1;",
        '#[cfg(any(kani, feature = "support"))]\nconst PRODUCTION: usize = 1;',
    ):
        assert "PRODUCTION" in production_rust(retained), f"R9/E5 {retained!r}"
    projection = production_rust(
        '#[cfg(test)]\nuse fixture::Store;\n'
        'const PRODUCTION: &str = "kept";\n'
        '#[cfg(test)] mod tests { const LATE_FIXTURE: usize = 1; }\n'
        'const LATER_PRODUCTION: usize = 2;'
    )
    assert "fixture" not in projection and "LATE_FIXTURE" not in projection, "R6/E4"
    assert "PRODUCTION" in projection and "LATER_PRODUCTION" in projection, "R6/E5"
    attributed_field = production_rust(
        "struct Config {\n"
        "#[cfg(test)] fixture: Option<Result<u8, u16>>,\n"
        "live: usize,\n}"
    )
    assert "fixture" not in attributed_field and "live" in attributed_field, (
        "R6/E4 conditional field cannot hide later production"
    )
    for expression in ("1 < 2", "1 << 2"):
        comparison_or_shift = production_rust(
            f"#[cfg(test)] const TEST: bool = {expression};\n"
            'const LATER_PRODUCTION: &str = "kept";'
        )
        assert "TEST" not in comparison_or_shift, "R6/E4 test const removed"
        assert "LATER_PRODUCTION" in comparison_or_shift, (
            "R6/E5 comparison/shift cannot hide later production"
        )
    attributed_if = production_rust(
        "let publisher = {\n"
        "#[cfg(test)] if fixture { TestPublisher } else { TestFallback }\n"
        "#[cfg(not(test))] LivePublisher\n};"
    )
    assert "TestPublisher" not in attributed_if and "TestFallback" not in attributed_if, (
        "R8/E7 test-only if expression removed"
    )
    assert "LivePublisher" in attributed_if, "R8/E7 cfg(not(test)) sibling retained"
    attributed_block = production_rust(
        "fn configure() {\n"
        "#[cfg(test)] { fixture_setup(); }\n"
        "self.resource_reclamation = Some(repository);\n}"
    )
    assert "fixture_setup" not in attributed_block, "R8/E7 test-only block removed"
    assert "resource_reclamation" in attributed_block, (
        "R8/E7 ordinary assignment sibling retained"
    )
    commented = 'const URL: &str = "https://example.test/a/*b*/"; // fake\n/* nested /* fake */ code */\nconst LIVE: usize = 1;'
    uncommented = rust_without_comments(commented)
    assert uncommented.count("\n") == commented.count("\n"), "R7/E6 lines"
    assert "https://example.test/a/*b*/" in uncommented, "R7/E6 literal"
    assert "fake" not in uncommented and "LIVE" in uncommented, "R7/E6 comments"

    assert public_first_party_reexports(["pub use awaken_root::Thing;"]) == {
        "awaken_root"
    }, "R1"
    assert public_first_party_reexports(["", "pub use awaken_nested::{Thing};"]) == {
        "awaken_nested"
    }, "R2"
    assert public_first_party_reexports(["use awaken_private::Thing;"]) == set(), "R3"


def _awaken_metadata(manifest: dict) -> dict:
    return manifest.get("package", {}).get("metadata", {}).get("awaken", {})


def dependency_fitness_specs() -> list[_crate_dependency_fitness.CrateSpec]:
    """Build the one metadata-derived workspace dependency model."""
    specs: list[_crate_dependency_fitness.CrateSpec] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        metadata = _awaken_metadata(manifest)
        specs.append(
            _crate_dependency_fitness.CrateSpec(
                name=package_name(manifest),
                context=str(metadata.get("context", "")),
                layer=str(metadata.get("layer", "")),
                authority=str(metadata.get("authority", "")),
                normal_deps=frozenset(normal_dependency_names(manifest)),
                product=str(metadata.get("product", "")),
                bounded_context=str(metadata.get("bounded_context", "")),
            )
        )
    return specs


def architecture_fitness_specs() -> list[_arch_fitness.CrateSpec]:
    """Build filesystem-free architecture-fitness specs for every workspace crate."""
    specs: list[_arch_fitness.CrateSpec] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        awaken_metadata = _awaken_metadata(manifest)
        public_reexports = public_first_party_reexports(
            path.read_text(encoding="utf-8")
            for path in text_files(package_name(manifest))
        )
        specs.append(
            _arch_fitness.CrateSpec(
                name=package_name(manifest),
                normal_deps=frozenset(normal_dependency_names(manifest)),
                context=str(awaken_metadata.get("context", "")),
                layer=str(awaken_metadata.get("layer", "")),
                authority=str(awaken_metadata.get("authority", "")),
                public_first_party_reexports=public_reexports,
            )
        )
    return specs
