from __future__ import annotations

import asyncio
import contextlib
import importlib
import json
import os
import tempfile
import time
from pathlib import Path
from typing import Awaitable, Callable, TypeVar

import anthropic
from anthropic.lib.tools import ToolError, beta_async_tool
from anthropic.lib.tools.agent_toolset import AgentToolContext, beta_agent_toolset_20260401


BASE_URL = os.environ["AWAKEN_MANAGED_BASE_URL"]
ORACLE_PATH = Path(os.environ["AWAKEN_PYTHON_ORACLE"])
ENVIRONMENT_KEY = "e2e-env-key"  # awaken-allow: secret
T = TypeVar("T")


async def drain(page: Awaitable[object]) -> list[object]:
    return [item async for item in await page]


async def expect_tool_error(awaitable: Awaitable[object], failure: str) -> None:
    try:
        await awaitable
    except ToolError:
        return
    raise AssertionError(failure)


def exercise_public_library_surface() -> None:
    # Public-surface graph: C1=the generated oracle binds exact wheel __all__
    # identities; C2=this process imports the exact locked wheel. Effects:
    # E1=every reviewed symbol resolves from its advertised module; E2=missing,
    # extra, or relocated symbols fail before behavioral scenarios can mask an
    # import incompatibility. Types/constants stop here; their owning helpers
    # receive state-machine coverage below instead of redundant value tests.
    expected = json.loads(ORACLE_PATH.read_text(encoding="utf-8"))["current"]["library_exports"]
    by_module: dict[str, list[str]] = {}
    for identity in expected:
        module_name, _, export_name = identity.rpartition(".")
        by_module.setdefault(module_name, []).append(export_name)
    for module_name, export_names in by_module.items():
        module = importlib.import_module(module_name)
        assert sorted(module.__all__) == sorted(export_names)
        assert all(getattr(module, export_name) is not None for export_name in export_names)


async def eventually(
    observe: Callable[[], Awaitable[T]],
    accept: Callable[[T], bool],
    failure: str,
    *,
    task: asyncio.Task[None] | None = None,
) -> T:
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if task is not None and task.done():
            task.result()
        observed = await observe()
        if accept(observed):
            return observed
        await asyncio.sleep(0.04)
    raise AssertionError(failure)


async def wait_for_tool_boundary(
    client: anthropic.AsyncAnthropic,
    session_id: str,
    receipt_id: str,
) -> list[object]:
    async def observe() -> list[object]:
        return await drain(client.beta.sessions.events.list(session_id, limit=2))

    def accepted(events: list[object]) -> bool:
        receipt = next((index for index, event in enumerate(events) if event.id == receipt_id), None)
        if receipt is None:
            return False
        suffix = events[receipt:]
        return any(event.type == "agent.custom_tool_use" for event in suffix) and any(
            event.type == "session.status_idle"
            and event.stop_reason is not None
            and event.stop_reason.type == "requires_action"
            for event in suffix
        )

    return await eventually(observe, accepted, "Python Session tool boundary was not committed")


async def create_tool_session(
    client: anthropic.AsyncAnthropic,
    environment_id: str,
    title: str,
) -> tuple[object, str]:
    session = await client.beta.sessions.create(
        agent="assistant",
        environment_id=environment_id,
        title=title,
    )
    receipt = await client.beta.sessions.events.send(
        session.id,
        events=[{
            "type": "user.message",
            "content": [{"type": "text", "text": "answer it"}],
        }],
    )
    receipt_id = receipt.data[0].id
    await wait_for_tool_boundary(client, session.id, receipt_id)
    return session, receipt_id


@beta_async_tool(name="submit_answer")
async def submit_answer(question: str) -> str:
    """Answer the deterministic fixture question.

    Args:
        question: The question selected by the fixture model.
    """
    assert question == "what is 6 x 7?"
    return "42"


async def exercise_poller(client: anthropic.AsyncAnthropic) -> None:
    # Poller causal graph: C1=self-hosted Environment seeds healthcheck;
    # C2=Session creation adds work; C3=the official helper uses only its
    # scoped Bearer client; C4=drain+auto_stop owns claim/ack/yield/stop.
    # Effects: E1=both exact work kinds are yielded active and acknowledged;
    # E2=no queued/active lease remains. Missing auth scoping, query defaults,
    # acknowledgement, or generator-finally cleanup breaks P1 C1..C4=>E1+E2.
    environment = await client.beta.environments.create(
        name="python-poller",
        config={"type": "self_hosted"},
    )
    session = await client.beta.sessions.create(
        agent="assistant",
        environment_id=environment.id,
        title="python-poller-session",
    )
    seen = []
    async for work in client.beta.environments.work.poller(
        environment_id=environment.id,
        environment_key=ENVIRONMENT_KEY,
        drain=True,
        block_ms=None,
    ):
        assert work.state == "active"
        seen.append((work.data.type, getattr(work.data, "id", None)))
    assert {kind for kind, _ in seen} == {"healthcheck", "session"}
    assert ("session", session.id) in seen
    remaining = await drain(client.beta.environments.work.list(environment.id))
    assert all(work.state not in {"queued", "active"} for work in remaining)
    assert all(work.acknowledged_at is not None for work in remaining)


async def exercise_tool_runner(client: anthropic.AsyncAnthropic) -> None:
    # SessionToolRunner decision table across the Python-only implementation:
    # R1 owned+success -> execute once, post one typed result; R2 owned+raise ->
    # post one error result without crashing; R3 unowned -> observe but post
    # nothing. All paths start from one durable requires_action boundary and
    # finish by re-listing committed events, so an in-memory-only success,
    # duplicate dispatch, fabricated result, or wrong result union fails.
    environment = await client.beta.environments.create(
        name="python-tool-runner",
        config={"type": "self_hosted"},
    )

    successful, _ = await create_tool_session(client, environment.id, "python-tool-success")
    calls = [
        call
        async for call in client.beta.sessions.events.tool_runner(
            successful.id,
            tools=[submit_answer],
            max_idle=0.05,
        )
    ]
    assert len(calls) == 1
    assert calls[0].name == "submit_answer" and calls[0].posted and not calls[0].is_error
    assert calls[0].result is not None and calls[0].result["type"] == "user.custom_tool_result"
    events = await drain(client.beta.sessions.events.list(successful.id))
    assert sum(event.type == "user.custom_tool_result" for event in events) == 1

    failing, _ = await create_tool_session(client, environment.id, "python-tool-error")

    @beta_async_tool(name="submit_answer")
    async def fail_tool(question: str) -> str:
        """Fail the deterministic fixture.

        Args:
            question: The question selected by the fixture model.
        """
        raise RuntimeError(f"refusing {question}")

    failed_calls = [
        call
        async for call in client.beta.sessions.events.tool_runner(
            failing.id,
            tools=[fail_tool],
            max_idle=0.05,
        )
    ]
    assert len(failed_calls) == 1
    assert failed_calls[0].posted and failed_calls[0].is_error
    failed_events = await drain(client.beta.sessions.events.list(failing.id))
    error_results = [event for event in failed_events if event.type == "user.custom_tool_result"]
    assert len(error_results) == 1 and error_results[0].is_error

    unowned, _ = await create_tool_session(client, environment.id, "python-tool-unowned")
    observed_calls = []
    observed_event = asyncio.Event()

    async def consume_unowned() -> None:
        async for call in client.beta.sessions.events.tool_runner(
            unowned.id,
            tools=[],
            max_idle=0,
        ):
            observed_calls.append(call)
            observed_event.set()

    consumer = asyncio.create_task(consume_unowned())
    await asyncio.wait_for(observed_event.wait(), timeout=20)
    unowned_events = await drain(client.beta.sessions.events.list(unowned.id))
    assert all(event.type != "user.custom_tool_result" for event in unowned_events)
    # An independent executor answers the unowned call. This proves the runner
    # did not fabricate a result while still giving its stream a natural next
    # terminal boundary in the same owning task (no generator abandonment).
    tool_use = next(event for event in unowned_events if event.type == "agent.custom_tool_use")
    await client.beta.sessions.events.send(
        unowned.id,
        events=[{
            "type": "user.custom_tool_result",
            "custom_tool_use_id": tool_use.id,
            "content": [{"type": "text", "text": "externally handled"}],
        }],
    )
    await asyncio.wait_for(consumer, timeout=20)
    assert len(observed_calls) == 1
    observed = observed_calls[0]
    assert observed.name == "submit_answer" and not observed.posted and observed.result is None
    completed = await drain(client.beta.sessions.events.list(unowned.id))
    assert sum(event.type == "user.custom_tool_result" for event in completed) == 1
    await client.beta.sessions.delete(unowned.id)


async def exercise_environment_worker(client: anthropic.AsyncAnthropic, workdir: Path) -> None:
    # Full-worker causal graph: C1=healthcheck is drained; C2=a Session is
    # durably parked on one custom tool; C3=EnvironmentWorker claims that exact
    # item; C4=heartbeat and SessionToolRunner run concurrently; C5=the result
    # resumes the Run. Effects: E1=one ack+heartbeat on the lease; E2=one local
    # execution and one committed result; E3=force-stop settles the item while
    # the outer worker remains cancellable. This is the cross-plane proof that
    # poller/tool_runner unit successes compose through Awaken's real API.
    environment = await client.beta.environments.create(
        name="python-environment-worker",
        config={"type": "self_hosted"},
    )
    async for _ in client.beta.environments.work.poller(
        environment_id=environment.id,
        environment_key=ENVIRONMENT_KEY,
        drain=True,
        block_ms=None,
    ):
        pass
    session, _ = await create_tool_session(client, environment.id, "python-full-worker")
    queued = await drain(client.beta.environments.work.list(environment.id))
    work = next(item for item in queued if item.data.type == "session" and item.data.id == session.id)
    executions = 0

    @beta_async_tool(name="submit_answer")
    async def counted_tool(question: str) -> str:
        """Count and answer the deterministic fixture.

        Args:
            question: The question selected by the fixture model.
        """
        nonlocal executions
        executions += 1
        return "42"

    worker = client.beta.environments.work.worker(
        environment_id=environment.id,
        environment_key=ENVIRONMENT_KEY,
        tools=[counted_tool],
        workdir=workdir,
        max_idle=0.05,
        memory_sync_interval=None,
    )
    running = asyncio.create_task(worker.run())
    try:
        settled = await eventually(
            lambda: client.beta.environments.work.retrieve(
                work.id,
                environment_id=environment.id,
            ),
            lambda item: item.state == "stopped",
            "Python EnvironmentWorker did not settle its Session work",
            task=running,
        )
        assert settled.acknowledged_at is not None and settled.latest_heartbeat_at is not None
        events = await drain(client.beta.sessions.events.list(session.id))
        assert executions == 1
        assert sum(event.type == "user.custom_tool_result" for event in events) == 1
        assert any(
            event.type == "session.status_idle"
            and event.stop_reason is not None
            and event.stop_reason.type == "end_turn"
            for event in events
        )
    finally:
        running.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await running


async def exercise_agent_toolset() -> None:
    # Local toolset partition: registration owns six exact names; write/read/
    # edit share one confined workdir; an over-limit whole read fails while an
    # exact bounded range succeeds; traversal fails before filesystem access.
    # These checks own Python helper behavior only—the server-side tool result
    # lifecycle is owned by the runner/worker cases above.
    with tempfile.TemporaryDirectory(prefix="awaken-python-toolset-") as temporary:
        root = Path(temporary)
        async with AgentToolContext(workdir=root, max_file_bytes=32) as context:
            tools = {tool.name: tool for tool in beta_agent_toolset_20260401(context)}
            assert list(tools) == ["bash", "read", "write", "edit", "glob", "grep"]
            await tools["write"].call({"file_path": "note.txt", "content": "hello world"})
            assert await tools["read"].call({"file_path": "note.txt"}) == "hello world"
            await tools["edit"].call({
                "file_path": "note.txt",
                "old_string": "world",
                "new_string": "there",
            })
            assert (root / "note.txt").read_text(encoding="utf-8") == "hello there"
            (root / "large.txt").write_text("\n".join(f"line-{index}" for index in range(20)), encoding="utf-8")
            assert await tools["read"].call({
                "file_path": "large.txt",
                "view_range": [10, 10],
            }) == "line-9"
            await expect_tool_error(
                tools["read"].call({"file_path": "large.txt"}),
                "whole over-limit Python read was accepted",
            )
            await expect_tool_error(
                tools["read"].call({"file_path": "../escape"}),
                "Python toolset traversal escaped its workdir",
            )


async def main() -> None:
    assert anthropic.__version__ == "1.2.0"
    exercise_public_library_surface()
    await exercise_agent_toolset()
    async with anthropic.AsyncAnthropic(
        api_key="e2e-dummy",  # awaken-allow: secret
        base_url=BASE_URL,
        max_retries=0,
    ) as client:
        await exercise_poller(client)
        await exercise_tool_runner(client)
        with tempfile.TemporaryDirectory(prefix="awaken-python-worker-") as temporary:
            await exercise_environment_worker(client, Path(temporary))
    print(
        "PYTHON HELPERS PASS: poller, tool_runner success/error/unowned, "
        "EnvironmentWorker composition, AgentToolContext/toolset"
    )


if __name__ == "__main__":
    asyncio.run(main())
