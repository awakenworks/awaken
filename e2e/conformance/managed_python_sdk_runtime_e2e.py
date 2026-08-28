from __future__ import annotations

import asyncio
import base64
import hashlib
import hmac
import importlib.metadata
import inspect
import json
import os
import time
from pathlib import Path

import anthropic
import httpx2
from standardwebhooks import WebhookVerificationError


BASE_URL = os.environ["AWAKEN_MANAGED_BASE_URL"]
LOCK = Path(os.environ["AWAKEN_PYTHON_REQUIREMENTS_LOCK"])
PYTHON_ORACLE = Path(__file__).resolve().parents[2] / (
    "contracts/anthropic-managed/python-upstream-oracle.generated.json"
)
MANAGED_BETA = "managed-agents-2026-04-01"
SKILL_V1 = b"---\nname: python-greeter\ndescription: Python SDK v1\n---\nSay hi."
SKILL_V2 = b"---\nname: python-greeter\ndescription: Python SDK v2\n---\nSay hi warmly."


def assert_locked_environment() -> None:
    # Dependency-closure partition: every non-comment lock row must resolve to
    # its exact installed distribution version; an ambient/missing/different
    # package fails before any HTTP request. This makes the client under test an
    # exact input, not whatever happens to be installed on the host.
    for row in LOCK.read_text(encoding="utf-8").splitlines():
        if not row or row.startswith("#"):
            continue
        name, expected = row.split("==", 1)
        actual = importlib.metadata.version(name)
        assert actual == expected, f"{name}: installed={actual}, expected={expected}"


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
        "environment_id": "fixture",
        "events": [{"type": "user.message", "content": [{"type": "text", "text": "fixture"}]}],
        "file": ("fixture.txt", b"fixture", "text/plain"),
        "files": [("SKILL.md", SKILL_V1, "text/markdown")],
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


def resource_method(client: anthropic.Anthropic, operation_id: str) -> object:
    parts = operation_id.split(".")
    resource: object = client
    for part in parts[:-1]:
        resource = getattr(resource, part)
    return getattr(getattr(resource, "with_raw_response"), parts[-1])


def normalized_actual_path(path: str) -> str:
    segments = path.split("/")
    return "/".join("{}" if segment == "fixture" else segment for segment in segments)


def exercise_all_operation_requests() -> None:
    # Complete request-construction graph: C1=the exact Python wheel exposes
    # each of the generated 127 methods; C2=all required arguments come from one
    # reviewed, fail-closed fixture vocabulary; C3=with_raw_response performs
    # the real SDK transform/serialization without coupling this client test to
    # 127 duplicate service fixtures. Effects: E1=each call emits exactly one
    # request with the oracle verb/path/query/beta set; E2=unknown new required
    # parameters or a missing/extra request fail. Decision table:
    # C1+C2+C3=>E1; !C1||!C2||request_count!=1||coordinate drift=>E2.
    # Response DTO semantics are exercised by the real-process scenarios below
    # and the shared TypeScript behavior owners, not by this raw-response sweep.
    oracle = json.loads(PYTHON_ORACLE.read_text(encoding="utf-8"))
    requests: list[httpx2.Request] = []

    def respond(request: httpx2.Request) -> httpx2.Response:
        requests.append(request)
        return httpx2.Response(200, json={})

    transport = httpx2.MockTransport(respond)
    http_client = httpx2.Client(transport=transport)
    with anthropic.Anthropic(
        api_key="request-sweep",  # awaken-allow: secret
        http_client=http_client,
        max_retries=0,
    ) as client:
        for operation in oracle["current"]["operations"]:
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
                    raise AssertionError(f"{operation['id']}: unsupported parameter kind {parameter.kind}")
            before = len(requests)
            response = method(*positional, **keyword)
            assert response.status_code == 200
            assert len(requests) == before + 1, f"{operation['id']}: exact request count"
            request = requests[-1]
            assert request.method == operation["method"], operation["id"]
            assert normalized_actual_path(request.url.path) == operation["path"], operation["id"]
            expected_query = operation.get("transport_query", "")
            assert request.url.query.decode() == expected_query, operation["id"]
            actual_betas = sorted(filter(None, request.headers.get("anthropic-beta", "").split(",")))
            assert actual_betas == operation["betas"], operation["id"]
    assert len(requests) == len(oracle["current"]["operations"]) == 127


def wait_for_idle(client: anthropic.Anthropic, session_id: str, receipt_id: str) -> list[object]:
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        # A limit of two forces the official cursor iterator across multiple
        # pages; the exact receipt fences this Run from any earlier history.
        events = list(client.beta.sessions.events.list(session_id, limit=2))
        receipt = next((index for index, event in enumerate(events) if event.id == receipt_id), None)
        if receipt is not None:
            suffix = events[receipt:]
            if any(event.type == "agent.message" for event in suffix) and any(
                event.type == "session.status_idle" for event in suffix
            ):
                return suffix
        time.sleep(0.05)
    raise AssertionError("sync Python Session did not reach idle")


def exercise_sync_session() -> None:
    # Sync Session causal graph: create with SDK-default beta -> exact send
    # receipt -> cursor pagination -> canonical lifecycle -> SSE replay -> 404
    # classification -> delete. Decision table: every edge must decode through
    # Python models; missing default selector, page loss, stream loss, malformed
    # error mapping, or stale deletion fails this single client-language owner.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=BASE_URL, max_retries=0) as client:
        session = client.beta.sessions.create(agent="assistant", environment_id="env_local")
        assert session.type == "session" and session.status == "idle"
        assert client.beta.sessions.retrieve(session.id).id == session.id
        receipt = client.beta.sessions.events.send(
            session.id,
            events=[{"type": "user.message", "content": [{"type": "text", "text": "python-sync"}]}],
        )
        suffix = wait_for_idle(client, session.id, receipt.data[0].id)
        assert next(event for event in suffix if event.type == "agent.message").content[0].text == (
            "Echo: python-sync"
        )
        with client.beta.sessions.events.stream(session.id) as stream:
            streamed = [event.type for event in stream]
        assert "agent.message" in streamed and "session.status_idle" in streamed
        try:
            client.beta.sessions.retrieve("sesn_python_missing")
        except anthropic.NotFoundError as error:
            assert error.status_code == 404
            assert error.body["error"]["type"] == "not_found_error"
        else:
            raise AssertionError("missing Session did not raise NotFoundError")
        deleted = client.beta.sessions.delete(session.id)
        assert deleted.id == session.id


async def wait_for_async_idle(
    client: anthropic.AsyncAnthropic,
    session_id: str,
    receipt_id: str,
) -> list[object]:
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        page = await client.beta.sessions.events.list(session_id, limit=1)
        events = [event async for event in page]
        receipt = next((index for index, event in enumerate(events) if event.id == receipt_id), None)
        if receipt is not None:
            suffix = events[receipt:]
            if any(event.type == "agent.message" for event in suffix) and any(
                event.type == "session.status_idle" for event in suffix
            ):
                return suffix
        await asyncio.sleep(0.05)
    raise AssertionError("async Python Session did not reach idle")


async def exercise_async_session() -> None:
    # Orthogonal async partition: repeat only the client-runtime edges whose
    # implementation differs—awaited CRUD/send, AsyncPageCursor and AsyncStream.
    # Service lifecycle assertions stay in the sync/shared owners, preventing a
    # redundant second semantic scenario while still detecting event-loop bugs.
    async with anthropic.AsyncAnthropic(
        api_key="e2e-dummy",
        base_url=BASE_URL,
        max_retries=0,
    ) as client:
        session = await client.beta.sessions.create(agent="assistant", environment_id="env_local")
        receipt = await client.beta.sessions.events.send(
            session.id,
            events=[{"type": "user.message", "content": [{"type": "text", "text": "python-async"}]}],
        )
        suffix = await wait_for_async_idle(client, session.id, receipt.data[0].id)
        assert any(event.type == "agent.message" for event in suffix)
        streamed = []
        async with await client.beta.sessions.events.stream(session.id) as stream:
            async for event in stream:
                streamed.append(event.type)
        assert "session.status_idle" in streamed
        await client.beta.sessions.delete(session.id)


def exercise_beta_ga_handoff() -> None:
    # Beta/GA change-point graph: Python 1.2 beta Files/Skills deliberately sends
    # `beta=true` without old dated pins and decodes GA-shaped DTOs. A resource
    # created through one root is retrieved/versioned/deleted through the other.
    # Effects: shared identity and persistence, distinct generated Python model
    # classes, multipart integrity, and cursor decoding. Any parallel store,
    # selector inference, or projection mismatch breaks the handoff.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=BASE_URL, max_retries=0) as client:
        first = client.beta.files.upload(file=("python-beta.txt", b"beta-to-ga", "text/plain"))
        assert first.type == "file"
        assert client.files.retrieve_metadata(first.id).id == first.id
        assert [item.id for item in client.beta.files.list(ids=[first.id])] == [first.id]
        assert client.files.delete(first.id).id == first.id

        second = client.files.upload(file=("python-ga.txt", b"ga-to-beta", "text/plain"))
        assert client.beta.files.retrieve_metadata(second.id).id == second.id
        assert client.beta.files.delete(second.id).id == second.id

        skill = client.beta.skills.create(
            files=[("SKILL.md", SKILL_V1, "text/markdown")],
            display_name="Python Greeter",
        )
        assert skill.type == "skill"
        assert client.skills.retrieve(skill.id).id == skill.id
        version = client.skills.versions.create(
            skill.id,
            files=[("SKILL.md", SKILL_V2, "text/markdown")],
        )
        assert version.id
        version_ids = [item.id for item in client.beta.skills.versions.list(skill.id)]
        assert version.id in version_ids and len(version_ids) == 2
        assert client.beta.skills.versions.retrieve(version.id, skill_id=skill.id).id == version.id
        assert client.skills.versions.delete(version.id, skill_id=skill.id).id == version.id
        assert client.skills.delete(skill.id).id == skill.id


def exercise_webhook_decoder() -> None:
    # Offline helper partition: exact valid signature decodes one typed event;
    # one-bit payload mutation rejects before DTO trust. This owns Python's
    # standardwebhooks adapter only; webhook resource semantics remain shared.
    payload = json.dumps({
        "id": "webhook_event_python",
        "type": "agent.created",
        "created_at": "2026-08-28T00:00:00Z",
        "data": {"id": "agent_python", "type": "agent"},
    }, separators=(",", ":"))
    secret_bytes = b"p" * 32
    secret = "whsec_" + base64.b64encode(secret_bytes).decode()
    webhook_id = "msg_python"
    timestamp = str(int(time.time()))
    signed = f"{webhook_id}.{timestamp}.{payload}".encode()
    signature = base64.b64encode(hmac.new(secret_bytes, signed, hashlib.sha256).digest()).decode()
    headers = {
        "webhook-id": webhook_id,
        "webhook-timestamp": timestamp,
        "webhook-signature": f"v1,{signature}",
    }
    client = anthropic.Anthropic(api_key="e2e-dummy", base_url=BASE_URL)
    event = client.beta.webhooks.unwrap(payload, headers=headers, key=secret)
    assert event.type == "agent.created" and event.data.id == "agent_python"
    try:
        client.beta.webhooks.unwrap(payload + " ", headers=headers, key=secret)
    except WebhookVerificationError as error:
        assert "signature" in str(error).lower()
    else:
        raise AssertionError("tampered Python webhook payload was accepted")
    client.close()


def main() -> None:
    assert_locked_environment()
    assert anthropic.__version__ == "1.2.0"
    exercise_all_operation_requests()
    exercise_sync_session()
    asyncio.run(exercise_async_session())
    exercise_beta_ga_handoff()
    exercise_webhook_decoder()
    print(
        "PYTHON SDK PASS: 127 request constructors; sync/async Session, cursor, SSE, "
        "errors, beta/GA handoff, webhook"
    )


if __name__ == "__main__":
    main()
