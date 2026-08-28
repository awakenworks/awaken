from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import hmac
import importlib.metadata
import json
import os
import time
from pathlib import Path

import anthropic
import httpx2
from anthropic.lib.sessions import accumulate_managed_agents_event
from anthropic.types.beta import (
    BetaManagedAgentsDeltaContent,
    BetaManagedAgentsDeltaEvent,
    BetaManagedAgentsStartEvent,
)
from anthropic.types.beta.sessions import BetaManagedAgentsTextBlock
from standardwebhooks import WebhookVerificationError

from managed_python_sdk_request_contract import (
    exercise_async_error_and_retry_contract,
    exercise_all_async_operation_requests as exercise_async_requests,
    exercise_all_operation_requests as exercise_requests,
    exercise_error_and_retry_contract,
)
from managed_python_sdk_installed_evidence import assert_installed_evidence


BASE_URL = os.environ["AWAKEN_MANAGED_BASE_URL"]
LOCK = Path(os.environ["AWAKEN_PYTHON_REQUIREMENTS_LOCK"])
PYTHON_ORACLE = Path(__file__).resolve().parents[2] / (
    "contracts/anthropic-managed/python-upstream-oracle.generated.json"
)
SCOPE = Path(__file__).resolve().parents[2] / "packages/managed-sdk-oracle/config/scope.json"
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
    assert_installed_evidence(
        anthropic.__version__,
        Path(anthropic.__file__).resolve().parent.parent,
        PYTHON_ORACLE,
        SCOPE,
    )


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
    exercise_requests(anthropic, httpx2, oracle["current"]["operations"])
    assert len(oracle["current"]["operations"]) == 127


async def exercise_all_async_operation_requests() -> None:
    # The current deep runtime matrix owns the same sync/async metamorphic
    # relation as every historical change point. Reading the generated oracle
    # here keeps 127 operation identities single-owned by the extractor.
    oracle = json.loads(PYTHON_ORACLE.read_text(encoding="utf-8"))
    await exercise_async_requests(anthropic, httpx2, oracle["current"]["operations"])


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
        # Disconnect after one decoded frame, then create a fresh official SDK
        # stream. Managed SSE is full replay: the second stream must contain the
        # first id exactly once and reach the same terminal facts. This catches
        # parser cleanup, accidental resume-only behavior, duplicate frames, or
        # a server cache that cannot replay committed history.
        with client.beta.sessions.events.stream(session.id) as stream:
            disconnected = next(iter(stream))
        with client.beta.sessions.events.stream(session.id) as stream:
            replayed = list(stream)
        replayed_ids = [event.id for event in replayed]
        assert disconnected.id in replayed_ids
        assert len(replayed_ids) == len(set(replayed_ids))
        replayed_types = [event.type for event in replayed]
        assert "agent.message" in replayed_types and "session.status_idle" in replayed_types
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
        accumulated = None
        async with await client.beta.sessions.events.stream(
            session.id,
            event_deltas=["agent.message"],
        ) as stream:
            async for event in stream:
                streamed.append(event.type)
                accumulated = accumulate_managed_agents_event(accumulated, event)
        assert "session.status_idle" in streamed
        assert accumulated is not None
        assert accumulated.type == "agent.message"
        assert accumulated.content[0].text == "Echo: python-async"
        await client.beta.sessions.delete(session.id)


def exercise_accumulator_contract() -> None:
    # Accumulator state-transition graph: S0=None; event_start(agent.message)
    # opens S1 without mutating input; two index-0 deltas append into S2/S3; a
    # future event is a no-op; delta-before-start and an index gap fail closed.
    # The live stream above additionally proves the official event_deltas list
    # query and canonical buffered-event replacement against Awaken.
    start = BetaManagedAgentsStartEvent(
        type="event_start",
        event={"id": "message_python_preview", "type": "agent.message"},
    )

    def delta(text: str, index: int = 0) -> BetaManagedAgentsDeltaEvent:
        return BetaManagedAgentsDeltaEvent(
            type="event_delta",
            event_id="message_python_preview",
            delta=BetaManagedAgentsDeltaContent(
                type="content_delta",
                index=index,
                content=BetaManagedAgentsTextBlock(type="text", text=text),
            ),
        )

    opened = accumulate_managed_agents_event(None, start)
    assert opened is not None and opened.content == []
    first = accumulate_managed_agents_event(opened, delta("hello"))
    second = accumulate_managed_agents_event(first, delta(" world"))
    assert first is not opened and second is not first
    assert opened.content == [] and first.content[0].text == "hello"
    assert second.content[0].text == "hello world"
    try:
        accumulate_managed_agents_event(None, delta("orphan"))
    except anthropic.AnthropicError as error:
        assert "before its event_start" in str(error)
    else:
        raise AssertionError("orphan Python event_delta was accepted")
    try:
        accumulate_managed_agents_event(opened, delta("gap", index=1))
    except anthropic.AnthropicError as error:
        assert "beyond the end" in str(error)
    else:
        raise AssertionError("out-of-order Python event_delta was accepted")


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
    unverified = client.beta.webhooks.parse_unverified(payload)
    assert unverified.type == "agent.created" and unverified.data.id == "agent_python"
    event = client.beta.webhooks.unwrap(payload, headers=headers, key=secret)
    assert event.type == "agent.created" and event.data.id == "agent_python"
    try:
        client.beta.webhooks.unwrap(payload + " ", headers=headers, key=secret)
    except WebhookVerificationError as error:
        assert "signature" in str(error).lower()
    else:
        raise AssertionError("tampered Python webhook payload was accepted")
    client.close()


def prepare_recovery(state_path: Path) -> None:
    # Recovery causal graph, phase A: C1=the exact Python 1.2 projections create
    # four independently persisted aggregates; C2=the Session receipt reaches a
    # terminal durable event batch before shutdown. Effect E1=only stable ids
    # and semantic expectations cross the process boundary—no Python object,
    # cursor, response cache, or in-memory server state can satisfy phase B.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=BASE_URL, max_retries=0) as client:
        session = client.beta.sessions.create(agent="assistant", environment_id="env_local")
        receipt = client.beta.sessions.events.send(
            session.id,
            events=[{"type": "user.message", "content": [{"type": "text", "text": "python-recovery"}]}],
        )
        suffix = wait_for_idle(client, session.id, receipt.data[0].id)
        assert next(event for event in suffix if event.type == "agent.message").content[0].text == (
            "Echo: python-recovery"
        )
        store = client.beta.memory_stores.create(name="python-recovery-store")
        memory = client.beta.memory_stores.memories.create(
            store.id,
            path="/recovery.md",
            content="durable-memory",
            view="full",
        )
        file = client.beta.files.upload(file=("recovery.txt", b"durable-file", "text/plain"))
        skill = client.beta.skills.create(
            files=[("SKILL.md", SKILL_V1, "text/markdown")],
            display_name="Python Recovery Skill",
        )
        state_path.write_text(
            json.dumps({
                "session_id": session.id,
                "receipt_id": receipt.data[0].id,
                "store_id": store.id,
                "memory_id": memory.id,
                "file_id": file.id,
                "skill_id": skill.id,
                "skill_version_id": skill.latest_version_id,
            }),
            encoding="utf-8",
        )


def verify_recovery(state_path: Path) -> None:
    # Recovery causal graph, phase B: C3=a fresh process opens the same durable
    # deployment; C4=GA and Beta projections address one File/Skill identity;
    # C5=Session history/SSE and Skill archives are reconstructed from stored
    # facts. Effects: E2=typed DTOs, page iteration, terminal order, archive
    # bytes, File download policy, and cross-projection identities survive;
    # E3=cleanup through the opposite
    # projection is immediately authoritative. Decision table:
    # C1+C2+C3+C4+C5=>E1+E2+E3; cache-only success, split repositories, missing
    # event batches, corrupt blobs, or stale projection all fail here.
    state = json.loads(state_path.read_text(encoding="utf-8"))
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=BASE_URL, max_retries=0) as client:
        session = client.beta.sessions.retrieve(state["session_id"])
        assert session.id == state["session_id"] and session.status == "idle"
        history = list(client.beta.sessions.events.list(session.id, limit=1))
        receipt_index = next(index for index, event in enumerate(history) if event.id == state["receipt_id"])
        suffix = history[receipt_index:]
        assert next(event for event in suffix if event.type == "agent.message").content[0].text == (
            "Echo: python-recovery"
        )
        assert any(event.type == "session.status_idle" for event in suffix)
        with client.beta.sessions.events.stream(session.id) as stream:
            streamed = [event.type for event in stream]
        assert "agent.message" in streamed and "session.status_idle" in streamed

        store = client.beta.memory_stores.retrieve(state["store_id"])
        memory = client.beta.memory_stores.memories.retrieve(
            state["memory_id"],
            memory_store_id=store.id,
            view="full",
        )
        assert memory.content == "durable-memory"
        assert [item.id for item in client.beta.memory_stores.memories.list(store.id)] == [memory.id]

        assert client.files.retrieve_metadata(state["file_id"]).id == state["file_id"]
        try:
            client.files.download(state["file_id"])
        except anthropic.BadRequestError as error:
            assert error.status_code == 400
            assert error.body["error"] == {
                "type": "invalid_request_error",
                "message": "file is not downloadable",
            }
        else:
            raise AssertionError("ordinary Managed File became downloadable after restart")
        skill = client.skills.retrieve(state["skill_id"])
        assert skill.id == state["skill_id"]
        archived_skill = client.beta.skills.versions.download(
            state["skill_version_id"],
            skill_id=skill.id,
        ).read()
        assert SKILL_V1 in archived_skill

        client.beta.sessions.delete(session.id)
        client.beta.memory_stores.memories.delete(memory.id, memory_store_id=store.id)
        client.beta.memory_stores.archive(store.id)
        client.beta.memory_stores.delete(store.id)
        client.beta.files.delete(state["file_id"])
        client.beta.skills.delete(skill.id)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", nargs="?", choices=("full", "prepare-recovery", "verify-recovery"), default="full")
    parser.add_argument("state", nargs="?", type=Path)
    args = parser.parse_args()
    assert_locked_environment()
    assert anthropic.__version__ == "1.2.0"
    if args.mode != "full":
        if args.state is None:
            raise AssertionError(f"{args.mode} requires a state path")
        if args.mode == "prepare-recovery":
            prepare_recovery(args.state)
        else:
            verify_recovery(args.state)
        print(f"PYTHON SDK RECOVERY {args.mode} PASS")
        return
    exercise_all_operation_requests()
    asyncio.run(exercise_all_async_operation_requests())
    exercise_error_and_retry_contract(anthropic, httpx2)
    asyncio.run(exercise_async_error_and_retry_contract(anthropic, httpx2))
    exercise_accumulator_contract()
    exercise_sync_session()
    asyncio.run(exercise_async_session())
    exercise_beta_ga_handoff()
    exercise_webhook_decoder()
    print(
        "PYTHON SDK PASS: 127 sync/async request constructors; sync/async Session and errors, "
        "cursor, SSE, event accumulation, beta/GA handoff, webhook helpers"
    )


if __name__ == "__main__":
    main()
