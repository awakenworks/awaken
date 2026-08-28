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
