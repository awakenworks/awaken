from __future__ import annotations

import asyncio
import copy
import json
import os
import typing
from collections.abc import Iterable
from pathlib import Path
from typing import Any

import anthropic
import httpx2
from pydantic import TypeAdapter

from managed_python_sdk_request_contract import (
    assert_operation_request,
    required_arguments,
)


CONTRACTS = os.environ["AWAKEN_MANAGED_PYTHON_RESPONSE_CONTRACTS"]
BINARY_BODY = b"managed-response-contract"

# Exact upstream Python 1.2 declaration variance. The official runtime keeps
# unknown literal values and the Python poller is separately exercised against
# both work kinds, but its generated `BetaSelfHostedWork.data` annotation still
# names only `BetaSessionWorkData`. Paths point to the original TS declaration
# node; selecting the session branch makes the remaining structure comparable.
# If Anthropic adds the missing union, changes the shape, or exposes another
# work-returning operation, this ledger fails and must be deliberately removed
# or extended after the real helper/runtime evidence is reviewed.
PYTHON_DECLARATION_VARIANCES: dict[str, tuple[str | int, ...]] = {
    "beta.environments.work.ack": ("properties", "data", "value"),
    "beta.environments.work.list": (
        "properties", "data", "value", "item", "properties", "data", "value"
    ),
    "beta.environments.work.poll": ("variants", 1, "properties", "data", "value"),
    "beta.environments.work.retrieve": ("properties", "data", "value"),
    "beta.environments.work.stop": ("properties", "data", "value"),
    "beta.environments.work.update": ("properties", "data", "value"),
}


def resource_method(client: object, operation_id: str) -> object:
    resource = client
    parts = operation_id.split(".")
    for part in parts[:-1]:
        resource = getattr(resource, part)
    return getattr(resource, parts[-1])


def return_kind(method: object) -> str:
    response_type = typing.get_type_hints(method)["return"]
    origin = typing.get_origin(response_type)
    identity = getattr(origin or response_type, "__name__", "")
    if identity in {"BinaryAPIResponse", "AsyncBinaryAPIResponse"}:
        return "binary"
    if identity in {"Stream", "AsyncStream"}:
        return "stream"
    return "json"


def declared_response_type(method: object) -> object:
    response_type = typing.get_type_hints(method)["return"]
    origin = typing.get_origin(response_type)
    if getattr(origin, "__name__", "") == "AsyncPaginator":
        arguments = typing.get_args(response_type)
        assert len(arguments) == 2
        return arguments[1]
    return response_type


def normalized_python_schema(schema: dict[str, Any]) -> dict[str, Any]:
    definitions = schema.get("$defs", {})

    def normalize(value: dict[str, Any]) -> dict[str, Any]:
        if "$ref" in value:
            prefix = "#/$defs/"
            reference = value["$ref"]
            assert reference.startswith(prefix), reference
            return normalize(definitions[reference.removeprefix(prefix)])
        alternatives = value.get("anyOf", value.get("oneOf"))
        if alternatives is not None:
            variants = []
            for alternative in alternatives:
                candidate = normalize(alternative)
                variants.extend(
                    candidate["variants"] if candidate["kind"] == "union" else [candidate]
                )
            unique = {
                json.dumps(candidate, sort_keys=True, separators=(",", ":")): candidate
                for candidate in variants
            }
            return {"kind": "union", "variants": list(unique.values())}
        if "const" in value:
            literal = value["const"]
            primitive = (
                "boolean" if isinstance(literal, bool)
                else "number" if isinstance(literal, (int, float))
                else "string"
            )
            return {"kind": "literal", "primitive": primitive, "value": literal}
        if "enum" in value:
            variants = [
                normalize({"const": literal})
                for literal in value["enum"]
            ]
            return variants[0] if len(variants) == 1 else {"kind": "union", "variants": variants}
        python_type = value.get("type")
        if isinstance(python_type, list):
            return normalize({"anyOf": [{"type": member} for member in python_type]})
        if python_type in {"string", "boolean", "null"}:
            return {"kind": python_type}
        if python_type in {"integer", "number"}:
            return {"kind": "number"}
        if python_type == "array":
            return {"kind": "array", "item": normalize(value.get("items", {}))}
        if python_type == "object":
            properties = {
                name: {
                    "required": name in value.get("required", []),
                    "value": normalize(property_),
                }
                for name, property_ in sorted(value.get("properties", {}).items())
            }
            additional = value.get("additionalProperties", False)
            if additional is True:
                # Anthropic BaseModel intentionally retains forward-compatible
                # unknown fields. Its declared properties remain closed for
                # cross-language ownership; a bare dict remains open JSON.
                normalized_additional: bool | dict[str, Any] = (
                    False if properties else {"kind": "open-json", "purpose": "python-dict"}
                )
            elif additional is False:
                normalized_additional = False
            else:
                normalized_additional = normalize(additional)
            return {
                "kind": "object",
                "properties": properties,
                "additional": normalized_additional,
            }
        if not value:
            return {"kind": "open-json", "purpose": "python-any"}
        raise AssertionError(f"unsupported Python JSON Schema node {value}")

    return normalize(schema)


def declaration_mismatch(
    expected: dict[str, Any],
    declared: dict[str, Any],
    path: str = "$",
    *,
    declared_optional: bool = False,
) -> str | None:
    def collapse_boolean_domain(value: dict[str, Any]) -> dict[str, Any]:
        if value["kind"] != "union":
            return value
        boolean_values = {
            variant.get("value")
            for variant in value["variants"]
            if variant.get("kind") == "literal" and variant.get("primitive") == "boolean"
        }
        if boolean_values != {False, True}:
            return value
        retained = [
            variant
            for variant in value["variants"]
            if not (variant.get("kind") == "literal" and variant.get("primitive") == "boolean")
        ]
        retained.append({"kind": "boolean"})
        return retained[0] if len(retained) == 1 else {"kind": "union", "variants": retained}

    expected = collapse_boolean_domain(expected)
    declared = collapse_boolean_domain(declared)

    if declared_optional and declared["kind"] == "union":
        expected_accepts_null = expected["kind"] == "null" or (
            expected["kind"] == "union"
            and any(variant["kind"] == "null" for variant in expected["variants"])
        )
        if not expected_accepts_null:
            without_null = [
                variant for variant in declared["variants"] if variant["kind"] != "null"
            ]
            declared = (
                without_null[0]
                if len(without_null) == 1
                else {"kind": "union", "variants": without_null}
            )
    if expected["kind"] != declared["kind"]:
        return f"{path}: expected {expected['kind']}, Python declares {declared['kind']}"
    kind = expected["kind"]
    if kind == "open-json":
        return None
    if kind == "literal":
        return None if expected == declared else f"{path}: literal differs"
    if kind in {"string", "number", "boolean", "null", "never"}:
        return None
    if kind == "array":
        return declaration_mismatch(expected["item"], declared["item"], f"{path}[]")
    if kind == "union":
        remaining = list(declared["variants"])
        for expected_variant in expected["variants"]:
            match = next(
                (
                    index
                    for index, declared_variant in enumerate(remaining)
                    if declaration_mismatch(expected_variant, declared_variant, path) is None
                ),
                None,
            )
            if match is None:
                return f"{path}: Python union lacks {expected_variant}"
            remaining.pop(match)
        if declared_optional:
            remaining = [variant for variant in remaining if variant["kind"] != "null"]
        return None if not remaining else f"{path}: Python union adds {remaining}"
    expected_names = set(expected["properties"])
    declared_names = set(declared["properties"])
    if expected_names != declared_names:
        return (
            f"{path}: property set differs; missing={sorted(expected_names - declared_names)}, "
            f"extra={sorted(declared_names - expected_names)}"
        )
    for name, expected_property in expected["properties"].items():
        declared_property = declared["properties"][name]
        if not expected_property["required"] and declared_property["required"]:
            return f"{path}.{name}: optional wire field is required by Python"
        mismatch = declaration_mismatch(
            expected_property["value"],
            declared_property["value"],
            f"{path}.{name}",
            declared_optional=not declared_property["required"],
        )
        if mismatch is not None:
            return mismatch
    expected_additional = expected["additional"]
    declared_additional = declared["additional"]
    if expected_additional is False or declared_additional is False:
        return (
            None
            if expected_additional is declared_additional
            else f"{path}.*: map openness differs"
        )
    # Both declarations intentionally own an open/map boundary. Purpose names
    # differ by language, while a finite map value must still agree exactly.
    if expected_additional["kind"] == "open-json":
        return None
    return declaration_mismatch(expected_additional, declared_additional, f"{path}.*")


def assert_declared_response(method: object, contract: dict[str, Any], operation_id: str) -> None:
    assert return_kind(method) == contract["kind"], operation_id
    if contract["kind"] != "json":
        return
    response_type = declared_response_type(method)
    declared = normalized_python_schema(TypeAdapter(response_type).json_schema())
    expected = copy.deepcopy(contract["schema"])
    variance = PYTHON_DECLARATION_VARIANCES.get(operation_id)
    if variance is not None:
        parent: Any = expected
        for component in variance[:-1]:
            parent = parent[component]
        key = variance[-1]
        work_data = parent[key]
        assert work_data["kind"] == "union", operation_id
        by_type = {
            variant["properties"]["type"]["value"]["value"]: variant
            for variant in work_data["variants"]
        }
        assert set(by_type) == {"healthcheck", "session"}, operation_id
        parent[key] = by_type["session"]
    mismatch = declaration_mismatch(expected, declared)
    assert mismatch is None, f"{operation_id}: {mismatch}"


def baseline(schema: dict[str, Any]) -> Any:
    kind = schema["kind"]
    if kind == "open-json":
        return {"fixture": True}
    if kind == "literal":
        return schema["value"]
    if kind == "string":
        # This remains a valid unconstrained TypeScript string while also
        # satisfying the sole narrower Python format in scope: date-time.
        return "2026-01-01T00:00:00Z"
    if kind == "number":
        return 1
    if kind == "boolean":
        return True
    if kind == "null":
        return None
    if kind == "array":
        return [baseline(schema["item"])]
    if kind == "object":
        value = {
            name: baseline(property_["value"])
            for name, property_ in schema["properties"].items()
        }
        additional = schema["additional"]
        if additional is not False:
            value["managed_additional_fixture"] = baseline(additional)
        return value
    if kind == "union":
        candidates = [variant for variant in schema["variants"] if variant["kind"] != "never"]
        if not candidates:
            raise AssertionError("official response union contains only never")
        preferred = next(
            (variant for variant in candidates if variant["kind"] not in {"null"}),
            candidates[0],
        )
        return baseline(preferred)
    if kind == "never":
        raise AssertionError("official response contract cannot materialize never")
    raise AssertionError(f"unsupported official response kind {kind}")


def distinct(values: Iterable[Any]) -> list[Any]:
    by_json = {json.dumps(value, sort_keys=True, separators=(",", ":")): value for value in values}
    return list(by_json.values())


def witnesses(schema: dict[str, Any]) -> list[Any]:
    """Produce one-at-a-time MC/DC witnesses for every finite type choice."""
    kind = schema["kind"]
    if kind == "never":
        return []
    if kind == "union":
        return distinct(
            witness
            for variant in schema["variants"]
            for witness in witnesses(variant)
        )

    base = baseline(schema)
    generated = [base]
    if kind == "array":
        generated.extend([[item] for item in witnesses(schema["item"])])
    elif kind == "object":
        for name, property_ in schema["properties"].items():
            nested = witnesses(property_["value"])
            for value in nested:
                changed = dict(base)
                changed[name] = value
                generated.append(changed)
            if not property_["required"]:
                omitted = dict(base)
                omitted.pop(name)
                generated.append(omitted)
        additional = schema["additional"]
        if additional is not False:
            for value in witnesses(additional):
                changed = dict(base)
                changed["managed_additional_fixture"] = value
                generated.append(changed)
    return distinct(generated)


def value_matches(value: Any, schema: dict[str, Any]) -> bool:
    kind = schema["kind"]
    if kind == "open-json":
        return value is None or isinstance(value, (bool, int, float, str, list, dict))
    if kind == "union":
        return any(value_matches(value, variant) for variant in schema["variants"])
    if kind == "literal":
        return type(value) is type(schema["value"]) and value == schema["value"]
    if kind == "never":
        return False
    if kind == "null":
        return value is None
    if kind == "boolean":
        return isinstance(value, bool)
    if kind == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == "string":
        return isinstance(value, str)
    if kind == "array":
        return isinstance(value, list) and all(
            value_matches(item, schema["item"]) for item in value
        )
    if kind != "object" or not isinstance(value, dict):
        return False
    properties = schema["properties"]
    if any(property_["required"] and name not in value for name, property_ in properties.items()):
        return False
    for name, nested in value.items():
        if name in properties:
            if not value_matches(nested, properties[name]["value"]):
                return False
        elif schema["additional"] is False or not value_matches(nested, schema["additional"]):
            return False
    return True


def decoded_json(value: object) -> Any:
    dump = getattr(value, "model_dump", None)
    if dump is not None:
        # Generated Python models materialize absent optional fields as None.
        # Validate the decoded wire fact rather than those client-only defaults;
        # otherwise omitting a non-nullable optional TS field appears to add a
        # null that was never present on the wire.
        return dump(mode="json", exclude_unset=True)
    return value


def response_for(contract: dict[str, Any], witness: Any) -> httpx2.Response:
    if contract["kind"] == "json":
        # `httpx2.Response(json=None)` means "no json argument" rather than a
        # JSON null body. Encode explicitly so nullable response branches cross
        # the same content-type and parser path as every other JSON witness.
        return httpx2.Response(
            200,
            content=json.dumps(witness, separators=(",", ":")).encode(),
            headers={"content-type": "application/json"},
        )
    if contract["kind"] == "binary":
        return httpx2.Response(
            200,
            content=BINARY_BODY,
            headers={"content-type": "application/octet-stream"},
        )
    return httpx2.Response(
        200,
        content=b"",
        headers={"content-type": "text/event-stream"},
    )


def operation_witnesses(contract: dict[str, Any]) -> list[Any]:
    return witnesses(contract["schema"]) if contract["kind"] == "json" else [None]


def assert_witness_design() -> None:
    # Harness mutation graph: a required discriminated choice and an optional
    # nullable choice are independent causes. The generated set must make both
    # required literals observable, make null/string observable, and omit the
    # optional field once. The validator must reject missing required, wrong
    # literal, and extra closed fields. These assertions prevent a simplifying
    # bug in the test generator from turning 3,827 calls into vacuous baselines.
    schema = {
        "kind": "object",
        "properties": {
            "choice": {
                "required": True,
                "value": {
                    "kind": "union",
                    "variants": [
                        {"kind": "literal", "primitive": "string", "value": "a"},
                        {"kind": "literal", "primitive": "string", "value": "b"},
                    ],
                },
            },
            "optional": {
                "required": False,
                "value": {
                    "kind": "union",
                    "variants": [{"kind": "null"}, {"kind": "string"}],
                },
            },
        },
        "additional": False,
    }
    generated = witnesses(schema)
    assert {value["choice"] for value in generated} == {"a", "b"}
    assert any("optional" not in value for value in generated)
    assert {value.get("optional") for value in generated} >= {
        None,
        "2026-01-01T00:00:00Z",
    }
    assert all(value_matches(value, schema) for value in generated)
    assert not value_matches({"optional": None}, schema)
    assert not value_matches({"choice": "c"}, schema)
    assert not value_matches({"choice": "a", "extension": True}, schema)


def exercise_sync(operations: list[dict[str, Any]], contracts: dict[str, Any]) -> int:
    current: dict[str, Any] = {}
    requests: list[object] = []

    def respond(request: object) -> httpx2.Response:
        requests.append(request)
        return response_for(current["contract"], current["witness"])

    count = 0
    with anthropic.Anthropic(
        api_key="response-contract-sync",  # awaken-allow: secret
        http_client=httpx2.Client(transport=httpx2.MockTransport(respond)),
        max_retries=0,
    ) as client:
        for operation in operations:
            contract = contracts[operation["id"]]
            method = resource_method(client, operation["id"])
            assert_declared_response(method, contract, operation["id"])
            positional, keyword = required_arguments(method)
            for witness in operation_witnesses(contract):
                current.update(contract=contract, witness=witness)
                before = len(requests)
                parsed = method(*positional, **keyword)
                assert len(requests) == before + 1, operation["id"]
                assert_operation_request(operation, requests[-1])
                if contract["kind"] == "json":
                    assert value_matches(decoded_json(parsed), contract["schema"]), operation["id"]
                elif contract["kind"] == "binary":
                    assert parsed.read() == BINARY_BODY, operation["id"]
                    parsed.close()
                else:
                    assert list(parsed) == [], operation["id"]
                    parsed.close()
                count += 1
    return count


async def exercise_async(operations: list[dict[str, Any]], contracts: dict[str, Any]) -> int:
    current: dict[str, Any] = {}
    requests: list[object] = []

    async def respond(request: object) -> httpx2.Response:
        requests.append(request)
        return response_for(current["contract"], current["witness"])

    count = 0
    async with anthropic.AsyncAnthropic(
        api_key="response-contract-async",  # awaken-allow: secret
        http_client=httpx2.AsyncClient(transport=httpx2.MockTransport(respond)),
        max_retries=0,
    ) as client:
        for operation in operations:
            contract = contracts[operation["id"]]
            method = resource_method(client, operation["id"])
            assert_declared_response(method, contract, operation["id"])
            positional, keyword = required_arguments(method)
            for witness in operation_witnesses(contract):
                current.update(contract=contract, witness=witness)
                before = len(requests)
                parsed = await method(*positional, **keyword)
                assert len(requests) == before + 1, operation["id"]
                assert_operation_request(operation, requests[-1])
                if contract["kind"] == "json":
                    assert value_matches(decoded_json(parsed), contract["schema"]), operation["id"]
                elif contract["kind"] == "binary":
                    assert await parsed.read() == BINARY_BODY, operation["id"]
                    await parsed.close()
                else:
                    assert [event async for event in parsed] == [], operation["id"]
                    await parsed.close()
                count += 1
    return count


def main() -> None:
    # Cross-language response causal graph:
    # C1 every Python 1.2 generated operation maps injectively to the reviewed
    # TypeScript 0.122 declaration for the same public protocol; C2 the schema
    # generator includes every field and one independent witness for every
    # nested union branch and optional-field omission; C3 real sync and async
    # clients execute their generated method, transport, media dispatch, and
    # response converter. Effects: E1 all 127 return annotations agree on JSON,
    # binary, or stream; E2 every finite response alternative decodes and still
    # satisfies the official declaration; E3 request identity remains exact.
    # Any unmapped operation, new response kind, missing converter, DTO drift,
    # sync/async asymmetry, or branch that cannot decode fails closed.
    bundle = json.loads(Path(CONTRACTS).read_text(encoding="utf-8"))
    assert anthropic.__version__ == bundle["python_version"]
    operations = bundle["operations"]
    contracts = bundle["contracts"]
    assert len(operations) == len(contracts) == 127
    assert set(contracts) == {operation["id"] for operation in operations}
    assert set(PYTHON_DECLARATION_VARIANCES) < set(contracts)
    assert_witness_design()
    media = {kind: 0 for kind in ("json", "binary", "stream")}
    for contract in contracts.values():
        media[contract["kind"]] += 1
    assert media == {"json": 122, "binary": 3, "stream": 2}
    sync_count = exercise_sync(operations, contracts)
    async_count = asyncio.run(exercise_async(operations, contracts))
    assert sync_count == async_count
    assert sync_count > len(operations)
    print(
        "Python response-contract PASS: "
        f"127 operations, {sync_count} MC/DC witnesses per client mode, media={media}, "
        f"reviewed_upstream_type_variances={len(PYTHON_DECLARATION_VARIANCES)}."
    )


if __name__ == "__main__":
    main()
