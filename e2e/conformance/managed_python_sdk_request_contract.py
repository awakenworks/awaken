from __future__ import annotations

import asyncio
import inspect
from typing import Any


SKILL = b"---\nname: python-fixture\ndescription: request fixture\n---\nFixture."


def required_fixture(name: str) -> object:
    fixtures: dict[str, object] = {
        "agent": "fixture",
        "auth": {
            "type": "static_bearer",
            "token": "fixture-token",  # awaken-allow: secret
            "mcp_server_url": "https://mcp.invalid",
        },
        "authorization_token": "fixture-token",  # awaken-allow: secret
        "ca_certificate_pem": "-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----",
        "content": "fixture",
        "display_name": "Fixture",
        "display_title": "Fixture",
        "environment_id": "fixture",
        "events": [{"type": "user.message", "content": [{"type": "text", "text": "fixture"}]}],
        "file": ("fixture.txt", b"fixture", "text/plain"),
        "files": [("SKILL.md", SKILL, "text/markdown")],
        "initial_events": [
            {"type": "user.message", "content": [{"type": "text", "text": "fixture"}]}
        ],
        "inputs": [{"type": "memory_store", "memory_store_id": "fixture"}],
        "metadata": {"fixture": "value"},
        "model": "claude-opus-5",
        "name": "Fixture",
        "path": "/fixture.md",
        "type": "file",
        "version": "fixture",
    }
    if name.endswith("_id"):
        return "fixture"
    try:
        return fixtures[name]
    except KeyError as error:
        raise AssertionError(f"no reviewed Python request fixture for required parameter {name}") from error


def resource_method(client: object, operation_id: str) -> object:
    parts = operation_id.split(".")
    resource = client
    for part in parts[:-1]:
        resource = getattr(resource, part)
    return getattr(getattr(resource, "with_raw_response"), parts[-1])


def required_arguments(method: object) -> tuple[list[object], dict[str, object]]:
    positional = []
    keyword = {}
    for parameter in inspect.signature(method).parameters.values():
        if parameter.default is not inspect.Parameter.empty:
            continue
        value = required_fixture(parameter.name)
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        ):
            positional.append(value)
        elif parameter.kind == inspect.Parameter.KEYWORD_ONLY:
            keyword[parameter.name] = value
        else:
            raise AssertionError(
                f"unsupported required parameter kind {parameter.kind} for {parameter.name}"
            )
    return positional, keyword


def normalized_actual_path(path: str) -> str:
    return "/".join("{}" if segment == "fixture" else segment for segment in path.split("/"))


def assert_operation_request(operation: dict[str, Any], request: object) -> None:
    assert request.method == operation["method"], operation["id"]
    assert normalized_actual_path(request.url.path) == operation["path"], operation["id"]
    assert request.url.query.decode() == operation.get("transport_query", ""), operation["id"]
    actual_betas = sorted(filter(None, request.headers.get("anthropic-beta", "").split(",")))
    assert actual_betas == operation["betas"], operation["id"]


def exercise_all_operation_requests(
    anthropic_module: Any,
    transport_module: Any,
    operations: list[dict[str, Any]],
) -> None:
    # Complete request-construction graph shared by current and historical
    # wheels: C1=the exact wheel exposes every extracted method; C2=all required
    # arguments come from one fail-closed fixture vocabulary; C3=its own raw
    # response adapter performs the real transform/serialization. Effects:
    # E1=one request per operation with exact verb/path/query/betas; E2=a new
    # required argument, missing method, duplicate request, or drift fails. DTO
    # and persistence semantics remain in real-process owners, so this helper
    # centralizes wire construction without cloning service state machines.
    requests = []

    def respond(request: object) -> object:
        requests.append(request)
        return transport_module.Response(200, json={})

    transport = transport_module.MockTransport(respond)
    http_client = transport_module.Client(transport=transport)
    with anthropic_module.Anthropic(
        api_key="request-sweep",  # awaken-allow: secret
        http_client=http_client,
        max_retries=0,
    ) as client:
        for operation in operations:
            method = resource_method(client, operation["id"])
            positional, keyword = required_arguments(method)
            before = len(requests)
            response = method(*positional, **keyword)
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation['id']}: exact request count"
            assert_operation_request(operation, requests[-1])
    assert len(requests) == len(operations)


async def exercise_all_async_operation_requests(
    anthropic_module: Any,
    transport_module: Any,
    operations: list[dict[str, Any]],
) -> None:
    # Metamorphic client-mode graph: C1=the same exact wheel and generated
    # operation ledger; C2=sync versus async resource implementation; C3=one
    # shared fail-closed fixture vocabulary. E1=both modes emit the identical
    # method/path/query/beta identity for every operation; E2=a missing async
    # method, new required parameter, duplicate request, or async-only selector
    # drift fails. Shared argument and request assertions make client mode the
    # sole transformed variable instead of maintaining a second inventory.
    requests = []

    async def respond(request: object) -> object:
        requests.append(request)
        return transport_module.Response(200, json={})

    transport = transport_module.MockTransport(respond)
    http_client = transport_module.AsyncClient(transport=transport)
    async with anthropic_module.AsyncAnthropic(
        api_key="async-request-sweep",  # awaken-allow: secret
        http_client=http_client,
        max_retries=0,
    ) as client:
        for operation in operations:
            method = resource_method(client, operation["id"])
            positional, keyword = required_arguments(method)
            before = len(requests)
            response = await method(*positional, **keyword)
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation['id']}: exact async request count"
            assert_operation_request(operation, requests[-1])
    assert len(requests) == len(operations)


def canonical_error(status: int, error_type: str) -> dict[str, object]:
    return {
        "type": "error",
        "error": {"type": error_type, "message": f"fixture {status}"},
        "request_id": f"req_body_{status}",
    }


async def _call_session_create(
    anthropic_module: Any,
    transport_module: Any,
    handler: object,
    *,
    asynchronous: bool,
    max_retries: int,
    raw: bool = False,
    extra_headers: dict[str, str] | None = None,
) -> object:
    arguments = {"agent": "fixture", "environment_id": "fixture"}
    if extra_headers is not None:
        arguments["extra_headers"] = extra_headers
    if asynchronous:
        async def async_handler(request: object) -> object:
            return handler(request)

        async with anthropic_module.AsyncAnthropic(
            api_key="async-transport-contract",  # awaken-allow: secret
            http_client=transport_module.AsyncClient(
                transport=transport_module.MockTransport(async_handler)
            ),
            max_retries=max_retries,
        ) as client:
            method = (
                client.beta.sessions.with_raw_response.create
                if raw
                else client.beta.sessions.create
            )
            return await method(**arguments)

    with anthropic_module.Anthropic(
        api_key="transport-contract",  # awaken-allow: secret
        http_client=transport_module.Client(transport=transport_module.MockTransport(handler)),
        max_retries=max_retries,
    ) as client:
        method = (
            client.beta.sessions.with_raw_response.create
            if raw
            else client.beta.sessions.create
        )
        return method(**arguments)


async def _exercise_error_and_retry_contract_for_mode(
    anthropic_module: Any,
    transport_module: Any,
    *,
    asynchronous: bool,
) -> None:
    # Transport decision table shared across the exact Python version matrix:
    # C1=one canonical Managed error envelope and request-id header; C2=status
    # is 400/401/403/404/409/413/422/429/500/529; C3=max_retries=0. Effects: E1=the
    # exact SDK exception subclass/status/type/request-id is observable; E2=one
    # request only. Retry decision table: C4=status/override selects retry or
    # rejection and C5=max_retries=2; E3=exactly three or one attempts. Mutation
    # relation: C6=one retry with an explicit idempotency key and body; E4=the
    # complete command identity is byte-stable. This owns Python transport
    # behavior only; Awaken's production error mapping is owned by deployed/Rust
    # operation cases. C7 projects the same decision graph through sync and
    # async API clients; the transport handler and all expectations remain
    # single-owned, so a mode-specific divergence cannot be normalized away.
    cases = (
        (400, "invalid_request_error", "BadRequestError"),
        (401, "authentication_error", "AuthenticationError"),
        (403, "permission_error", "PermissionDeniedError"),
        (404, "not_found_error", "NotFoundError"),
        (409, "conflict_error", "ConflictError"),
        (413, "request_too_large", "RequestTooLargeError"),
        (422, "invalid_request_error", "UnprocessableEntityError"),
        (429, "rate_limit_error", "RateLimitError"),
        (500, "api_error", "InternalServerError"),
        (529, "overloaded_error", "OverloadedError"),
    )
    for status, error_type, class_name in cases:
        requests = []

        def reject(request: object) -> object:
            requests.append(request)
            return transport_module.Response(
                status,
                request=request,
                json=canonical_error(status, error_type),
                headers={"request-id": f"req_header_{status}"},
            )

        try:
            await _call_session_create(
                anthropic_module,
                transport_module,
                reject,
                asynchronous=asynchronous,
                max_retries=0,
            )
        except anthropic_module.APIStatusError as error:
            assert error.__class__.__name__ == class_name
            assert error.status_code == status
            assert error.body["error"]["type"] == error_type
            assert error.request_id == f"req_header_{status}"
        else:
            raise AssertionError(f"{status}: Python SDK accepted a Managed error")
        assert len(requests) == 1, f"{status}: client fault retried"

    retry_cases = (
        (400, False, None),
        (408, True, None),
        (409, True, None),
        (413, False, None),
        (422, False, None),
        (429, True, None),
        (500, True, None),
        (529, True, None),
        (400, True, "true"),
        (500, False, "false"),
    )
    for status, retries, override in retry_cases:
        attempts = []

        def decide(request: object) -> object:
            attempts.append(request)
            if retries and len(attempts) == 3:
                return transport_module.Response(200, request=request, json={})
            headers = {"retry-after-ms": "0"}
            if override is not None:
                headers["x-should-retry"] = override
            return transport_module.Response(
                status,
                request=request,
                json=canonical_error(status, "api_error"),
                headers=headers,
            )

        try:
            response = await _call_session_create(
                anthropic_module,
                transport_module,
                decide,
                asynchronous=asynchronous,
                max_retries=2,
                raw=True,
            )
        except anthropic_module.APIStatusError as error:
            assert not retries, f"{status}/{override}: retryable response was rejected"
            assert error.status_code == status
        else:
            assert retries, f"{status}/{override}: non-retryable response was accepted"
            assert response.status_code == 200
        assert len(attempts) == (3 if retries else 1), (
            f"{status}/{override}: exact retry bound"
        )

    attempts = []

    def transient(request: object) -> object:
        attempts.append(request)
        if len(attempts) == 1:
            return transport_module.Response(
                500,
                request=request,
                json=canonical_error(500, "api_error"),
                headers={"request-id": "req_retry_1", "retry-after-ms": "0"},
            )
        return transport_module.Response(200, request=request, json={})

    key = "python-managed-retry-identity"
    response = await _call_session_create(
        anthropic_module,
        transport_module,
        transient,
        asynchronous=asynchronous,
        max_retries=1,
        raw=True,
        extra_headers={"idempotency-key": key},
    )
    assert response.status_code == 200
    assert len(attempts) == 2
    first, second = attempts
    assert (first.method, first.url, first.content) == (second.method, second.url, second.content)
    assert first.headers["idempotency-key"] == second.headers["idempotency-key"] == key


def exercise_error_and_retry_contract(anthropic_module: Any, transport_module: Any) -> None:
    asyncio.run(_exercise_error_and_retry_contract_for_mode(
        anthropic_module,
        transport_module,
        asynchronous=False,
    ))


async def exercise_async_error_and_retry_contract(
    anthropic_module: Any,
    transport_module: Any,
) -> None:
    await _exercise_error_and_retry_contract_for_mode(
        anthropic_module,
        transport_module,
        asynchronous=True,
    )
