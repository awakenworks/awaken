#!/usr/bin/env python3
"""Prepare and verify the JSON-Schema-derived TypeScript contract."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import TypeAlias, cast


JsonScalar: TypeAlias = None | bool | int | float | str
JsonValue: TypeAlias = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]
JsonObject: TypeAlias = dict[str, JsonValue]

# These aliases preserve names already consumed by the console. They contain no
# independently authored fields: every alias still resolves to the canonical
# Rust/JSON-Schema-owned type.
COMPATIBILITY_ALIASES: dict[str, str] = {
    "APIDialect": "ApiDialect",
    "APIError": "ApiError",
    "PrimaryElement": "ProfileCandidate",
}
SCHEMA_ANNOTATION_KEYS = {
    "$comment",
    "default",
    "deprecated",
    "description",
    "examples",
    "readOnly",
    "title",
    "writeOnly",
}


class ContractCodegenError(ValueError):
    """The schema graph or generated TypeScript violated a codegen invariant."""


def _read_object(path: Path) -> JsonObject:
    value: object = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ContractCodegenError(f"{path} must contain a JSON object")
    return cast(JsonObject, value)


def _schema_identity(node: JsonObject) -> JsonObject:
    """Return the assertion-bearing part used to compare duplicate definitions."""

    return {
        key: value
        for key, value in node.items()
        if key not in {"$schema", "$defs", "title"}
    }


def _definition_name(fallback: str, node: JsonObject) -> str:
    title = node.get("title")
    return title if isinstance(title, str) and title else fallback


def _normalize_ref_siblings(value: JsonValue) -> JsonValue:
    """Keep $ref annotations without making json2ts clone the referenced type."""

    if isinstance(value, list):
        return [_normalize_ref_siblings(item) for item in value]
    if not isinstance(value, dict):
        return value

    normalized = {
        key: _normalize_ref_siblings(item) for key, item in value.items()
    }
    if normalized and set(normalized).issubset(SCHEMA_ANNOTATION_KEYS):
        # An annotation-only JSON Schema accepts every value. json2ts otherwise
        # narrows it to an object merely because an annotation is present.
        return {**normalized, "tsType": "unknown"}
    if "$ref" not in normalized or len(normalized) == 1:
        return normalized

    reference = normalized.pop("$ref")
    # JSON Schema 2020-12 permits annotation siblings beside `$ref`. Expressing
    # the same assertion through `allOf` makes json-schema-to-typescript retain
    # the property annotation without emitting a suffixed duplicate definition.
    return {**normalized, "allOf": [{"$ref": reference}]}


def _merge_definition(
    definitions: dict[str, JsonObject],
    owners: dict[str, str],
    name: str,
    node: JsonObject,
    owner: str,
    *,
    replace_equivalent: bool = False,
) -> None:
    existing = definitions.get(name)
    if existing is not None and _schema_identity(existing) != _schema_identity(node):
        raise ContractCodegenError(
            f"conflicting JSON Schema definition {name!r}: "
            f"{owners[name]} versus {owner}"
        )
    if existing is None or replace_equivalent:
        definitions[name] = node
        owners[name] = owner


def build_codegen_schema(bundle_path: Path, output_path: Path) -> None:
    """Merge the exported roots into one canonical, deduplicated `$defs` graph."""

    bundle = _read_object(bundle_path)
    schemas = bundle.get("schemas")
    if not isinstance(schemas, dict) or not schemas:
        raise ContractCodegenError(f"{bundle_path} must contain a non-empty schemas map")

    definitions: dict[str, JsonObject] = {}
    owners: dict[str, str] = {}

    for root_name, untyped_schema in schemas.items():
        if not isinstance(root_name, str) or not isinstance(untyped_schema, dict):
            raise ContractCodegenError("schema names and values must be JSON objects")
        schema = cast(JsonObject, untyped_schema)
        nested = schema.get("$defs", {})
        if not isinstance(nested, dict):
            raise ContractCodegenError(f"schema {root_name!r} has a non-object $defs")
        for nested_name, untyped_node in nested.items():
            if not isinstance(nested_name, str) or not isinstance(untyped_node, dict):
                raise ContractCodegenError(
                    f"schema {root_name!r} has an invalid nested definition"
                )
            node = cast(JsonObject, untyped_node)
            canonical_name = _definition_name(nested_name, node)
            _merge_definition(
                definitions,
                owners,
                canonical_name,
                node,
                f"{root_name}::$defs::{nested_name}",
            )

    for root_name, untyped_schema in schemas.items():
        schema = cast(JsonObject, untyped_schema)
        canonical_name = _definition_name(root_name, schema)
        root_definition = {
            key: value
            for key, value in schema.items()
            if key not in {"$schema", "$defs"}
        }
        _merge_definition(
            definitions,
            owners,
            canonical_name,
            root_definition,
            f"root::{root_name}",
            replace_equivalent=True,
        )

    for alias, target in COMPATIBILITY_ALIASES.items():
        if alias in definitions:
            raise ContractCodegenError(
                f"compatibility alias {alias!r} collides with a schema-owned definition"
            )
        if target not in definitions:
            raise ContractCodegenError(
                f"compatibility alias {alias!r} targets missing definition {target!r}"
            )
        definitions[alias] = {
            "title": alias,
            "allOf": [{"$ref": f"#/$defs/{target}"}],
        }

    normalized = cast(dict[str, JsonObject], _normalize_ref_siblings(definitions))
    output = {
        "$comment": (
            "Codegen-only merged view of model-schemas.generated.json; "
            "the exported JSON Schema remains authoritative."
        ),
        "$defs": normalized,
    }
    output_path.write_text(
        json.dumps(output, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )


def normalize_schema(input_path: Path, output_path: Path) -> None:
    """Normalize one standalone schema through the same reference policy."""

    normalized = _normalize_ref_siblings(_read_object(input_path))
    output_path.write_text(
        json.dumps(normalized, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )


def finalize_stdin_typescript(path: Path) -> None:
    """Replace json2ts's stdin placeholder without inventing a root DTO."""

    typescript = path.read_text(encoding="utf-8")
    placeholder = "by `undefined`'s JSON-Schema"
    if placeholder not in typescript:
        raise ContractCodegenError(
            "json2ts stdin output no longer contains the expected comment placeholder"
        )
    path.write_text(
        typescript.replace(placeholder, "by the generated JSON Schema"),
        encoding="utf-8",
    )


def _tagged_union(node: JsonObject) -> tuple[str, list[str]] | None:
    branches = node.get("oneOf")
    if not isinstance(branches, list) or len(branches) < 2:
        return None
    if not all(isinstance(branch, dict) and branch.get("type") == "object" for branch in branches):
        return None

    candidates: set[str] | None = None
    branch_constants: list[dict[str, str]] = []
    for untyped_branch in branches:
        branch = cast(JsonObject, untyped_branch)
        required = branch.get("required", [])
        properties = branch.get("properties", {})
        if not isinstance(required, list) or not isinstance(properties, dict):
            return None
        constants = {
            name: property_schema["const"]
            for name, property_schema in properties.items()
            if isinstance(name, str)
            and isinstance(property_schema, dict)
            and isinstance(property_schema.get("const"), str)
            and name in required
        }
        branch_constants.append(constants)
        candidates = set(constants) if candidates is None else candidates & set(constants)

    if not candidates:
        return None
    if len(candidates) != 1:
        raise ContractCodegenError(
            f"tagged union has ambiguous discriminators: {sorted(candidates)}"
        )
    discriminator = next(iter(candidates))
    values = [constants[discriminator] for constants in branch_constants]
    if len(values) != len(set(values)):
        raise ContractCodegenError(
            f"tagged union discriminator {discriminator!r} has duplicate constants"
        )
    return discriminator, values


def _exported_type_statement(typescript: str, name: str) -> str | None:
    start_match = re.search(rf"^export type {re.escape(name)}\s*=", typescript, re.MULTILINE)
    if start_match is None:
        return None
    next_export = re.search(
        r"^export (?:interface|type)\s+", typescript[start_match.end() :], re.MULTILINE
    )
    end = len(typescript)
    if next_export is not None:
        end = start_match.end() + next_export.start()
    return typescript[start_match.start() : end]


def assert_generated_contract(schema_path: Path, typescript_path: Path) -> None:
    schema = _read_object(schema_path)
    definitions = schema.get("$defs")
    if not isinstance(definitions, dict):
        raise ContractCodegenError(f"{schema_path} must contain an object $defs")
    typescript = typescript_path.read_text(encoding="utf-8")

    definition_names = {
        name for name in definitions if isinstance(name, str)
    }
    exported_names = set(
        re.findall(
            r"^export (?:interface|type)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            typescript,
            re.MULTILINE,
        )
    )
    missing = sorted(definition_names - exported_names)
    unexpected = sorted(exported_names - definition_names)
    if missing or unexpected:
        raise ContractCodegenError(
            "TypeScript exports must match the merged schema definitions exactly; "
            f"missing={missing}, unexpected={unexpected}"
        )

    checked = 0
    # Cause/effect graph: C1=a named schema is `oneOf`; C2=every object branch
    # requires one shared discriminator; C3=each discriminator value is a unique
    # string constant. Effects: E1=the named TS export is a union (not a flattened
    # interface); E2=it has one object branch per schema branch; E3=all constants
    # remain branch-local literals. Decision table: R1 C1+C2+C3 -> require
    # E1+E2+E3; R2 !C1 or !C2 -> ordinary schema, no tagged-union assertion;
    # R3 C1+C2+!C3 -> reject the ambiguous source graph before accepting output.
    for name, untyped_node in definitions.items():
        if not isinstance(name, str) or not isinstance(untyped_node, dict):
            raise ContractCodegenError("generated $defs must contain named objects")
        tagged = _tagged_union(cast(JsonObject, untyped_node))
        if tagged is None:
            continue
        discriminator, values = tagged
        statement = _exported_type_statement(typescript, name)
        if statement is None:
            raise ContractCodegenError(
                f"tagged schema {name!r} was not emitted as an exported TypeScript union"
            )
        if re.search(rf"^export interface {re.escape(name)}\b", typescript, re.MULTILINE):
            raise ContractCodegenError(f"tagged schema {name!r} was flattened to an interface")
        if statement.count("\n  | {") != len(values):
            raise ContractCodegenError(
                f"tagged schema {name!r} lost object branches: "
                f"expected {len(values)}"
            )
        for value in values:
            literal = json.dumps(value, ensure_ascii=False)
            if f"{discriminator}: {literal}" not in statement:
                raise ContractCodegenError(
                    f"tagged schema {name!r} lost {discriminator}={value!r}"
                )
        checked += 1

    if checked == 0:
        raise ContractCodegenError("the contract graph contained no tagged unions to verify")
    for alias, target in COMPATIBILITY_ALIASES.items():
        if f"export type {alias} = {target};" not in typescript:
            raise ContractCodegenError(
                f"missing shape-free compatibility alias {alias!r} -> {target!r}"
            )
    print(f"OK - preserved {checked} JSON-Schema tagged unions in TypeScript.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    build = subparsers.add_parser("build")
    build.add_argument("bundle", type=Path)
    build.add_argument("output", type=Path)

    normalize = subparsers.add_parser("normalize")
    normalize.add_argument("input", type=Path)
    normalize.add_argument("output", type=Path)

    finalize = subparsers.add_parser("finalize-stdin")
    finalize.add_argument("typescript", type=Path)

    check = subparsers.add_parser("assert")
    check.add_argument("schema", type=Path)
    check.add_argument("typescript", type=Path)

    args = parser.parse_args()
    try:
        if args.command == "build":
            build_codegen_schema(args.bundle, args.output)
        elif args.command == "normalize":
            normalize_schema(args.input, args.output)
        elif args.command == "finalize-stdin":
            finalize_stdin_typescript(args.typescript)
        else:
            assert_generated_contract(args.schema, args.typescript)
    except (ContractCodegenError, OSError, json.JSONDecodeError) as error:
        print(f"error: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
