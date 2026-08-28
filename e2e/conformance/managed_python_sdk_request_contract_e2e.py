from __future__ import annotations

import asyncio
import base64
import inspect
import json
import os
from pathlib import Path
from typing import Any

import anthropic
import httpx2

from managed_python_sdk_wire import normalized_name, semantic_request

DISCOVER_VARIANCES = os.environ.get("AWAKEN_MANAGED_REQUEST_DISCOVER_VARIANCES") == "1"

# Official-SDK request variance ledger. TypeScript 0.122 admits `null` for these
# query fields and serializes it as an empty query value; Python 1.2 admits the
# same value but omits the query pair. This is deliberately an exact set rather
# than a general null normalizer: adding/removing an SDK variance, moving the
# field, or changing any other wire component fails the gate and requires review.
KNOWN_NULL_QUERY_VARIANCES = frozenset({
    ("beta.agents.list", "page"),
    ("beta.agents.versions.list", "page"),
    ("beta.deployment_runs.list", "page"),
    ("beta.deployments.list", "page"),
    ("beta.dreams.list", "page"),
    ("beta.environments.list", "page"),
    ("beta.environments.work.heartbeat", "desired_ttl_seconds"),
    ("beta.environments.work.heartbeat", "expected_last_heartbeat"),
    ("beta.environments.work.list", "page"),
    ("beta.environments.work.poll", "block_ms"),
    ("beta.environments.work.poll", "reclaim_older_than_ms"),
    ("beta.files.list", "ids"),
    ("beta.files.list", "page"),
    ("beta.memory_stores.list", "page"),
    ("beta.memory_stores.memories.list", "page"),
    ("beta.memory_stores.memory_versions.list", "page"),
    ("beta.sessions.events.list", "page"),
    ("beta.sessions.list", "page"),
    ("beta.sessions.resources.list", "page"),
    ("beta.sessions.threads.events.list", "page"),
    ("beta.sessions.threads.list", "page"),
    ("beta.skills.list", "page"),
    ("beta.skills.list", "source"),
    ("beta.skills.versions.list", "page"),
    ("beta.tunnels.certificates.list", "page"),
    ("beta.tunnels.list", "page"),
    ("beta.user_profiles.list", "page"),
    ("beta.vaults.credentials.list", "page"),
    ("beta.vaults.list", "page"),
    ("files.list", "ids"),
    ("files.list", "page"),
    ("skills.list", "page"),
    ("skills.list", "source"),
    ("skills.versions.list", "page"),
})


def resource_method(client: object, operation_id: str) -> object:
    resource = client
    parts = operation_id.split(".")
    for part in parts[:-1]:
        resource = getattr(resource, part)
    return getattr(getattr(resource, "with_raw_response"), parts[-1])


def materialize(value: Any) -> Any:
    if isinstance(value, dict) and set(value) == {"__managed_sdk_upload__"}:
        upload = value["__managed_sdk_upload__"]
        return (
            upload["filename"],
            base64.b64decode(upload["content_base64"]),
            upload["media_type"],
        )
    if isinstance(value, list):
        return [materialize(item) for item in value]
    if isinstance(value, dict):
        return {name: materialize(nested) for name, nested in value.items()}
    return value


def call_arguments(method: object, provided: list[list[Any]]) -> tuple[list[Any], dict[str, Any]]:
    by_name = {normalized_name(name): materialize(value) for name, value in provided}
    assert len(by_name) == len(provided), "request argument projection is injective"
    positional: list[Any] = []
    keyword: dict[str, Any] = {}
    consumed: set[str] = set()
    signature_names: set[str] = set()
    for parameter in inspect.signature(method).parameters.values():
        name = normalized_name(parameter.name)
        assert name not in signature_names, f"ambiguous Python signature name {parameter.name}"
        signature_names.add(name)
        if name not in by_name:
            continue
        consumed.add(name)
        if parameter.kind == inspect.Parameter.POSITIONAL_ONLY:
            positional.append(by_name[name])
        elif parameter.kind in (
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
            inspect.Parameter.KEYWORD_ONLY,
        ):
            keyword[parameter.name] = by_name[name]
        else:
            raise AssertionError(
                f"unsupported Python request parameter kind {parameter.kind}: {parameter.name}"
            )
    assert consumed == set(by_name), (
        f"official Python method lacks request fields {sorted(set(by_name) - consumed)}"
    )
    return positional, keyword


def assert_request(
    actual: dict[str, Any],
    expected: dict[str, Any],
    operation_id: str,
    arguments: list[list[Any]],
    observed_variances: set[tuple[str, str]],
    unexpected_variances: list[dict[str, Any]],
) -> None:
    if actual == expected:
        return
    null_fields = [name for name, value in arguments if value is None]
    if len(null_fields) == 1:
        field = null_fields[0]
        coordinate = (operation_id, field)
        expected_without_empty_field = {
            **expected,
            "query": [
                pair
                for pair in expected["query"]
                if not (
                    pair[1] == ""
                    and normalized_name(pair[0]) == normalized_name(field)
                )
            ],
        }
        removed = len(expected["query"]) - len(expected_without_empty_field["query"])
        if (
            coordinate in KNOWN_NULL_QUERY_VARIANCES
            and removed == 1
            and actual == expected_without_empty_field
        ):
            observed_variances.add(coordinate)
            return
    if DISCOVER_VARIANCES:
        unexpected_variances.append({
            "actual": actual,
            "arguments": arguments,
            "expected": expected,
            "operation_id": operation_id,
        })
        return
    raise AssertionError(
        f"{operation_id}: official Python request differs from TypeScript wire authority\n"
        f"arguments={json.dumps(arguments, sort_keys=True, ensure_ascii=False)}\n"
        f"expected={json.dumps(expected, sort_keys=True, ensure_ascii=False)}\n"
        f"actual={json.dumps(actual, sort_keys=True, ensure_ascii=False)}"
    )


def empty_omission_coordinates(bundle: dict[str, Any]) -> frozenset[tuple[str, str, str]]:
    coordinates = [
        (
            witness["python_operation_id"],
            omission["field"],
            omission["wire_kind"],
        )
        for witness in bundle["witnesses"]
        if (omission := witness["python_empty_omission"]) is not None
    ]
    assert len(coordinates) == len(set(coordinates)), (
        "each derived Python empty-value omission has one causal witness"
    )
    return frozenset(coordinates)


def assert_witness_request(
    actual: dict[str, Any],
    witness: dict[str, Any],
    observed_null_variances: set[tuple[str, str]],
    observed_empty_omissions: set[tuple[str, str, str]],
    unexpected_variances: list[dict[str, Any]],
) -> None:
    operation_id = witness["python_operation_id"]
    omission = witness["python_empty_omission"]
    if omission is not None:
        # Metamorphic oracle: the official TS serializer proves both the
        # one-empty-field request and the otherwise identical omission request.
        # Python must equal the latter exactly. This admits no global empty-value
        # normalization and automatically fails closed when either SDK changes.
        assert actual == omission["expected"], (
            f"{operation_id}: Python {omission['wire_kind']} empty-value omission differs "
            "from the official TypeScript omission witness\n"
            f"field={omission['field']}\n"
            f"expected={json.dumps(omission['expected'], sort_keys=True, ensure_ascii=False)}\n"
            f"actual={json.dumps(actual, sort_keys=True, ensure_ascii=False)}"
        )
        observed_empty_omissions.add(
            (operation_id, omission["field"], omission["wire_kind"])
        )
        return
    assert_request(
        actual,
        witness["expected"],
        operation_id,
        witness["arguments"],
        observed_null_variances,
        unexpected_variances,
    )


def assert_empty_path_rejection(
    error: BaseException,
    rejection: dict[str, str],
    operation_id: str,
) -> None:
    assert type(error).__name__ == rejection["error_class"], operation_id
    field = rejection["field"]
    prefix = "Expected a non-empty value for `"
    suffix = "` but received ''"
    message = str(error)
    assert message.startswith(prefix) and message.endswith(suffix), operation_id
    python_field = message[len(prefix) : -len(suffix)]
    assert normalized_name(python_field) == normalized_name(field), operation_id


def variance_summary(variance: dict[str, Any]) -> dict[str, Any]:
    return {
        "actual_body": variance["actual"]["body"]["kind"],
        "actual_parts": len(variance["actual"]["body"].get("parts", [])),
        "actual_query": variance["actual"]["query"],
        "argument_names": [name for name, _ in variance["arguments"]],
        "null_fields": [name for name, value in variance["arguments"] if value is None],
        "expected_body": variance["expected"]["body"]["kind"],
        "expected_parts": len(variance["expected"]["body"].get("parts", [])),
        "expected_query": variance["expected"]["query"],
        "operation_id": variance["operation_id"],
    }


def exercise_sync(bundle: dict[str, Any]) -> tuple[int, int]:
    # Metamorphic relation P0: the exact Python 1.2 sync resource graph receives
    # the same language-neutral field witness that the official TS candidate
    # encoded. P1: httpx2 MockTransport observes the completed request after the
    # SDK's aliases, transforms, JSON/multipart encoder and headers. Effect E1:
    # path/query/header/body semantics equal the TS authority exactly; E2: a
    # missing field, wrong location, altered null/list/file encoding, extra
    # request or operation spelling fails at its first causal witness.
    requests = []
    invocations = 0
    observed_variances: set[tuple[str, str]] = set()
    observed_empty_omissions: set[tuple[str, str, str]] = set()
    unexpected_variances: list[dict[str, Any]] = []

    def respond(request: object) -> object:
        requests.append(request)
        return httpx2.Response(200, json={})

    with anthropic.Anthropic(
        api_key="sync-request-contract",  # awaken-allow: secret
        http_client=httpx2.Client(transport=httpx2.MockTransport(respond)),
        max_retries=0,
    ) as client:
        for witness in bundle["witnesses"]:
            operation_id = witness["python_operation_id"]
            method = resource_method(client, operation_id)
            positional, keyword = call_arguments(method, witness["arguments"])
            before = len(requests)
            invocations += 1
            try:
                response = method(*positional, **keyword)
            except ValueError as error:
                rejection = witness["python_rejection"]
                assert rejection is not None, f"{operation_id}: unexpected Python rejection"
                assert_empty_path_rejection(error, rejection, operation_id)
                assert len(requests) == before, f"{operation_id}: rejected before transport"
                continue
            assert witness["python_rejection"] is None, (
                f"{operation_id}: expected Python empty-path rejection vanished"
            )
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation_id}: exact sync request count"
            assert_witness_request(
                semantic_request(requests[-1]),
                witness,
                observed_variances,
                observed_empty_omissions,
                unexpected_variances,
            )
        # The official TS declaration admits `display_name: null` for multipart
        # Skill creation, but its own serializer rejects before fetch. Python
        # 1.2 accepts the same declared value and intentionally encodes it as
        # omission. Preserve that exact cross-language variance as evidence:
        # it may neither disappear silently nor expand to another field.
        for rejection in bundle["upstream_rejections"]:
            operation_id = rejection["python_operation_id"]
            method = resource_method(client, operation_id)
            positional, keyword = call_arguments(method, rejection["arguments"])
            before = len(requests)
            invocations += 1
            response = method(*positional, **keyword)
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation_id}: Python nullable multipart"
            assert_request(
                semantic_request(requests[-1]),
                rejection["python_expected"],
                operation_id,
                rejection["arguments"],
                observed_variances,
                unexpected_variances,
            )
    assert observed_variances == KNOWN_NULL_QUERY_VARIANCES, (
        "sync official-SDK null-query variance set changed: "
        f"missing={sorted(KNOWN_NULL_QUERY_VARIANCES - observed_variances)}, "
        f"added={sorted(observed_variances - KNOWN_NULL_QUERY_VARIANCES)}"
    )
    expected_empty_omissions = empty_omission_coordinates(bundle)
    assert observed_empty_omissions == expected_empty_omissions, (
        "sync official-SDK empty-value omission set changed: "
        f"missing={sorted(expected_empty_omissions - observed_empty_omissions)}, "
        f"added={sorted(observed_empty_omissions - expected_empty_omissions)}"
    )
    if DISCOVER_VARIANCES:
        print("SYNC REQUEST VARIANCES " + json.dumps(
            [variance_summary(variance) for variance in unexpected_variances],
            sort_keys=True,
            ensure_ascii=False,
        ))
    return invocations, len(requests)


async def exercise_async(bundle: dict[str, Any]) -> tuple[int, int]:
    # P2 changes only the official client runtime to AsyncAnthropic/AsyncClient.
    # The witness, normalized request and assertion stay single-owned, proving
    # sync/async parity without a duplicate expected corpus.
    requests = []
    invocations = 0
    observed_variances: set[tuple[str, str]] = set()
    observed_empty_omissions: set[tuple[str, str, str]] = set()
    unexpected_variances: list[dict[str, Any]] = []

    async def respond(request: object) -> object:
        requests.append(request)
        return httpx2.Response(200, json={})

    async with anthropic.AsyncAnthropic(
        api_key="async-request-contract",  # awaken-allow: secret
        http_client=httpx2.AsyncClient(transport=httpx2.MockTransport(respond)),
        max_retries=0,
    ) as client:
        for witness in bundle["witnesses"]:
            operation_id = witness["python_operation_id"]
            method = resource_method(client, operation_id)
            positional, keyword = call_arguments(method, witness["arguments"])
            before = len(requests)
            invocations += 1
            try:
                response = await method(*positional, **keyword)
            except ValueError as error:
                rejection = witness["python_rejection"]
                assert rejection is not None, f"{operation_id}: unexpected async Python rejection"
                assert_empty_path_rejection(error, rejection, operation_id)
                assert len(requests) == before, f"{operation_id}: async rejected before transport"
                continue
            assert witness["python_rejection"] is None, (
                f"{operation_id}: expected async Python empty-path rejection vanished"
            )
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation_id}: exact async request count"
            assert_witness_request(
                semantic_request(requests[-1]),
                witness,
                observed_variances,
                observed_empty_omissions,
                unexpected_variances,
            )
        for rejection in bundle["upstream_rejections"]:
            operation_id = rejection["python_operation_id"]
            method = resource_method(client, operation_id)
            positional, keyword = call_arguments(method, rejection["arguments"])
            before = len(requests)
            invocations += 1
            response = await method(*positional, **keyword)
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation_id}: async nullable multipart"
            assert_request(
                semantic_request(requests[-1]),
                rejection["python_expected"],
                operation_id,
                rejection["arguments"],
                observed_variances,
                unexpected_variances,
            )
    assert observed_variances == KNOWN_NULL_QUERY_VARIANCES, (
        "async official-SDK null-query variance set changed: "
        f"missing={sorted(KNOWN_NULL_QUERY_VARIANCES - observed_variances)}, "
        f"added={sorted(observed_variances - KNOWN_NULL_QUERY_VARIANCES)}"
    )
    expected_empty_omissions = empty_omission_coordinates(bundle)
    assert observed_empty_omissions == expected_empty_omissions, (
        "async official-SDK empty-value omission set changed: "
        f"missing={sorted(expected_empty_omissions - observed_empty_omissions)}, "
        f"added={sorted(observed_empty_omissions - expected_empty_omissions)}"
    )
    if DISCOVER_VARIANCES:
        print("ASYNC REQUEST VARIANCES " + json.dumps(
            [variance_summary(variance) for variance in unexpected_variances],
            sort_keys=True,
            ensure_ascii=False,
        ))
    return invocations, len(requests)


def main() -> None:
    contracts = Path(os.environ["AWAKEN_MANAGED_PYTHON_REQUEST_CONTRACTS"])
    bundle = json.loads(contracts.read_text(encoding="utf-8"))
    assert bundle["python_version"] == anthropic.__version__
    assert bundle["operation_count"] == 127
    assert len({witness["python_operation_id"] for witness in bundle["witnesses"]}) == 127
    sync_invocations, sync_requests = exercise_sync(bundle)
    async_invocations, async_requests = asyncio.run(exercise_async(bundle))
    expected_invocations = bundle["witness_count"] + len(bundle["upstream_rejections"])
    empty_path_rejections = sum(
        witness["python_rejection"] is not None for witness in bundle["witnesses"]
    )
    empty_omissions = empty_omission_coordinates(bundle)
    query_empty_omissions = sum(kind == "query" for _, _, kind in empty_omissions)
    multipart_empty_omissions = sum(kind == "multipart" for _, _, kind in empty_omissions)
    expected_requests = expected_invocations - empty_path_rejections
    assert sync_invocations == async_invocations == expected_invocations
    assert sync_requests == async_requests == expected_requests
    assert sync_invocations > bundle["operation_count"]
    print(
        "PYTHON REQUEST CONTRACT PASS: "
        f"{bundle['operation_count']} operations, {bundle['witness_count']} MC/DC parity witnesses "
        f"+ {len(KNOWN_NULL_QUERY_VARIANCES)} exact null-query variances "
        f"+ {query_empty_omissions} derived empty-query omissions "
        f"+ {multipart_empty_omissions} derived empty-multipart omissions "
        f"+ {empty_path_rejections} exact Python empty-path rejections "
        f"+ {len(bundle['upstream_rejections'])} exact upstream rejections per client mode, "
        "exact TS/Python path+query+header+JSON+multipart semantics"
    )


if __name__ == "__main__":
    main()
