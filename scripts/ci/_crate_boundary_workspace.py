"""Workspace discovery helpers for the crate-boundary fitness check."""

from __future__ import annotations

import re
import tomllib
from collections.abc import Iterable
from pathlib import Path

import _arch_fitness
import _crate_dependency_fitness


REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"


def load_manifest(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def package_name(manifest: dict) -> str:
    return str(manifest["package"]["name"])


def iter_crate_manifests() -> list[Path]:
    if not CRATES.exists():
        return []
    return sorted(CRATES.glob("*/*/Cargo.toml"))


def text_files(crate_name: str) -> list[Path]:
    manifest = next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None)
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


def rust_without_comments(source: str) -> str:
    """Blank Rust comments without changing offsets, lines, or string literals."""
    output: list[str] = []
    cursor = 0
    while cursor < len(source):
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


def rust_syntax(source: str) -> str:
    """Blank comments and literals while retaining Rust syntax and line structure."""
    source = rust_without_comments(source)
    output: list[str] = []
    cursor = 0
    while cursor < len(source):
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
    if predicate == "test":
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
    if cfg is None or re.search(r"\btest\b", cfg.group("predicate")) is None:
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


def selftest() -> None:
    """Cause/effect table for first-party facade discovery.

    Causes: C1 crate-root source; C2 nested-module source; C3 private import.
    Effects: E1 public first-party owner is reported; E2 private use is ignored.

    | Rule | root public | nested public | private | effect |
    | R1   | yes         | no            | no      | E1     |
    | R2   | no          | yes           | no      | E1     |
    | R3   | no          | no            | yes     | E2     |
    """
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
