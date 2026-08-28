from __future__ import annotations

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


def normalized_actual_path(path: str) -> str:
    return "/".join("{}" if segment == "fixture" else segment for segment in path.split("/"))


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
                        f"{operation['id']}: unsupported parameter kind {parameter.kind}"
                    )
            before = len(requests)
            response = method(*positional, **keyword)
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation['id']}: exact request count"
            request = requests[-1]
            assert request.method == operation["method"], operation["id"]
            assert normalized_actual_path(request.url.path) == operation["path"], operation["id"]
            assert request.url.query.decode() == operation.get("transport_query", ""), operation["id"]
            actual_betas = sorted(filter(None, request.headers.get("anthropic-beta", "").split(",")))
            assert actual_betas == operation["betas"], operation["id"]
    assert len(requests) == len(operations)


def canonical_error(status: int, error_type: str) -> dict[str, object]:
    return {
        "type": "error",
        "error": {"type": error_type, "message": f"fixture {status}"},
        "request_id": f"req_body_{status}",
    }


def exercise_error_and_retry_contract(anthropic_module: Any, transport_module: Any) -> None:
    # Transport decision table shared across the exact Python version matrix:
    # C1=one canonical Managed error envelope and request-id header; C2=status
    # is 400/401/403/404/409/422/429/500; C3=max_retries=0. Effects: E1=the
    # exact SDK exception subclass/status/type/request-id is observable; E2=one
    # request only. Retry graph: C4=500 then success, C5=max_retries=1,
    # C6=explicit idempotency key and request body. Effects: E3=two attempts;
    # E4=method/URL/body/idempotency identity is byte-stable. This owns Python
    # transport behavior only; Awaken's production error mapping is owned by
    # deployed/Rust operation cases.
    cases = (
        (400, "invalid_request_error", "BadRequestError"),
        (401, "authentication_error", "AuthenticationError"),
        (403, "permission_error", "PermissionDeniedError"),
        (404, "not_found_error", "NotFoundError"),
        (409, "conflict_error", "ConflictError"),
        (422, "invalid_request_error", "UnprocessableEntityError"),
        (429, "rate_limit_error", "RateLimitError"),
        (500, "api_error", "InternalServerError"),
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

        with anthropic_module.Anthropic(
            api_key="error-contract",  # awaken-allow: secret
            http_client=transport_module.Client(transport=transport_module.MockTransport(reject)),
            max_retries=0,
        ) as client:
            try:
                client.beta.sessions.create(agent="fixture", environment_id="fixture")
            except getattr(anthropic_module, class_name) as error:
                assert error.status_code == status
                assert error.body["error"]["type"] == error_type
                assert error.request_id == f"req_header_{status}"
            else:
                raise AssertionError(f"{status}: Python SDK accepted a Managed error")
        assert len(requests) == 1, f"{status}: client fault retried"

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
    with anthropic_module.Anthropic(
        api_key="retry-contract",  # awaken-allow: secret
        http_client=transport_module.Client(transport=transport_module.MockTransport(transient)),
        max_retries=1,
    ) as client:
        response = client.beta.sessions.with_raw_response.create(
            agent="fixture",
            environment_id="fixture",
            extra_headers={"idempotency-key": key},
        )
        assert response.status_code == 200
    assert len(attempts) == 2
    first, second = attempts
    assert (first.method, first.url, first.content) == (second.method, second.url, second.content)
    assert first.headers["idempotency-key"] == second.headers["idempotency-key"] == key
