from __future__ import annotations

import argparse
import asyncio
import importlib
import inspect
import json
import os
import time
from pathlib import Path

import anthropic

from managed_python_sdk_request_contract import (
    exercise_declared_request_witnesses,
    exercise_async_error_and_retry_contract,
    exercise_error_and_retry_contract,
    exercise_pathlike_upload_change_point,
)
from managed_python_sdk_installed_evidence import assert_installed_evidence
from managed_python_sdk_response_contract_e2e import exercise_async, exercise_sync


REPO = Path(__file__).resolve().parents[2]
ORACLE_PATH = Path(os.environ["AWAKEN_PYTHON_ORACLE"])
RESPONSE_CONTRACTS_PATH = Path(
    os.environ["AWAKEN_MANAGED_PYTHON_RESPONSE_CONTRACTS"]
)
SCOPE_PATH = REPO / "packages/managed-sdk-oracle/config/scope.json"


def extracted_evidence(version: str) -> tuple[dict[str, object], dict[str, object]]:
    installed_root = Path(anthropic.__file__).resolve().parent.parent
    return assert_installed_evidence(version, installed_root, ORACLE_PATH, SCOPE_PATH)


def exercise_library_exports(exports: list[str]) -> None:
    # Import partition: each exact historical wheel must resolve every explicit
    # handwritten __all__ identity extracted from that same wheel. Generated
    # resources are covered by the request sweep; executable current helper
    # behavior is covered by the dedicated helper state machines.
    by_module: dict[str, list[str]] = {}
    for identity in exports:
        module_name, _, export_name = identity.rpartition(".")
        by_module.setdefault(module_name, []).append(export_name)
    for module_name, names in by_module.items():
        module = importlib.import_module(module_name)
        assert sorted(module.__all__) == sorted(names)
        assert all(getattr(module, name) is not None for name in names)


async def exercise_resource_helper_surface(helpers: list[str]) -> None:
    async with anthropic.AsyncAnthropic(api_key="surface-only", max_retries=0) as client:  # awaken-allow: secret
        for identity in helpers:
            value: object = client
            for segment in identity.split("."):
                value = getattr(value, segment)
            assert callable(value), identity


def exercise_current_response_compatibility(
    transport: object,
    operations: list[dict[str, object]],
) -> int:
    # Historical-forward-compatibility causal graph:
    # C1 the installed wheel and its generated operation inventory are pinned by
    # source and wheel hashes; C2 the response corpus is generated once from the
    # reviewed current TypeScript declaration, rather than copied into a second
    # historical fixture; C3 every response branch is injected through that
    # wheel's own sync/async transport, media dispatch, generated converter, and
    # return annotation. Effects: E1 every historical operation still consumes
    # the current service-line wire shape; E2 JSON/binary/SSE selection and the
    # outbound operation identity agree in both client modes. Missing operation,
    # changed media kind, rejected union branch, lost required field, converter
    # asymmetry, or request drift fails closed.
    #
    # Historical declarations are intentionally not required to equal the newest
    # declarations: additive fields are the compatibility condition being tested.
    # Their exact source/type surfaces are independently hash-bound by
    # `assert_installed_evidence`; this test proves executable consumption, while
    # the current-oracle test owns strict cross-language declaration equality.
    bundle = json.loads(RESPONSE_CONTRACTS_PATH.read_text(encoding="utf-8"))
    contracts = bundle["contracts"]
    operation_ids = {operation["id"] for operation in operations}
    missing = operation_ids - set(contracts)
    assert not missing, (
        f"current response corpus lacks historical operations: {sorted(missing)}"
    )
    selected = {
        operation_id: contracts[operation_id]
        for operation_id in operation_ids
    }
    sync_count = exercise_sync(
        anthropic,
        transport,
        operations,
        selected,
        verify_declarations=False,
    )
    async_count = asyncio.run(
        exercise_async(
            anthropic,
            transport,
            operations,
            selected,
            verify_declarations=False,
        )
    )
    assert sync_count == async_count
    assert sync_count >= len(operations)
    return sync_count


def wait_for_idle(client: object, session_id: str, receipt_id: str) -> list[object]:
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        events = list(client.beta.sessions.events.list(session_id, limit=1))
        receipt = next((index for index, event in enumerate(events) if event.id == receipt_id), None)
        if receipt is not None:
            suffix = events[receipt:]
            if any(event.type == "agent.message" for event in suffix) and any(
                event.type == "session.status_idle" for event in suffix
            ):
                return suffix
        time.sleep(0.04)
    raise AssertionError(f"{anthropic.__version__}: Session did not reach idle")


def exercise_session(base_url: str, stream_event_names: list[str]) -> None:
    # Historical lifecycle graph: exact version defaults -> create -> exact
    # receipt -> limit=1 auto-pagination -> typed message+idle -> delete. Every
    # selected row is a reviewed source/protocol/transport change point; running
    # all rows therefore detects generated default and DTO regressions without
    # sampling redundant patch releases.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=base_url, max_retries=0) as client:
        # Test design: every_historical_python_sdk_decodes_real_error_boundaries
        # Cause/effect graph: exact wheel -> generated sync request -> real
        # Managed parser/domain boundary -> wheel-owned status subclass + nested
        # Anthropic body. Decision table: zero page limit => 400/BadRequestError;
        # absent Session => 404/NotFoundError; success, wrong class, plain-text,
        # or wrong discriminator fails before the positive lifecycle can mask it.
        try:
            client.beta.sessions.list(limit=0)
        except anthropic.BadRequestError as error:
            assert error.status_code == 400
            assert error.body["type"] == "error"
            assert error.body["error"]["type"] == "invalid_request_error"
            assert error.body["error"]["message"]
        else:
            raise AssertionError(
                f"{anthropic.__version__}: zero Session page limit was accepted"
            )
        try:
            client.beta.sessions.retrieve("sesn_python_matrix_missing")
        except anthropic.NotFoundError as error:
            assert error.status_code == 404
            assert error.body["type"] == "error"
            assert error.body["error"]["type"] == "not_found_error"
            assert error.body["error"]["message"]
        else:
            raise AssertionError(
                f"{anthropic.__version__}: absent Session was accepted"
            )

        session = client.beta.sessions.create(agent="assistant", environment_id="env_local")
        try:
            assert client.beta.sessions.retrieve(session.id).id == session.id
            receipt = client.beta.sessions.events.send(
                session.id,
                events=[{
                    "type": "user.message",
                    "content": [{"type": "text", "text": f"python-{anthropic.__version__}"}],
                }],
            )
            suffix = wait_for_idle(client, session.id, receipt.data[0].id)
            text = next(event for event in suffix if event.type == "agent.message").content[0].text
            assert text == f"Echo: python-{anthropic.__version__}"
            history = list(client.beta.sessions.events.list(session.id, limit=1))
            assert len(history) > 1
            assert len({event.id for event in history}) == len(history)
            with client.beta.sessions.events.stream(session.id) as stream:
                streamed = list(stream)
            streamed_types = [event.type for event in streamed]
            managed_streaming = {"agent.message", "session.status_idle"}.issubset(
                stream_event_names
            )
            # Capability decision table: C1=the exact wheel's reviewed parser
            # dispatches both Managed event names; C2=Awaken replays canonical
            # named frames. C1+C2 => typed message+idle. !C1+C2 => the first
            # official Managed release intentionally filters those frames and
            # yields none. This records the upstream 0.92 boundary instead of
            # misdiagnosing it as a server replay failure or hanging forever.
            if managed_streaming:
                assert "agent.message" in streamed_types
                assert "session.status_idle" in streamed_types
            else:
                assert streamed_types == [], (
                    f"{anthropic.__version__}: legacy parser behavior changed: {streamed_types}"
                )
        finally:
            client.beta.sessions.delete(session.id)


def exercise_memory(base_url: str, operation_ids: set[str]) -> None:
    if "beta.memory_stores.create" not in operation_ids:
        return
    # Selector change-point graph: no explicit beta override is supplied. Each
    # wheel's generated default must address the same canonical repository and
    # decode create/retrieve/list/delete/archive across the live server.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=base_url, max_retries=0) as client:
        store = client.beta.memory_stores.create(name=f"python-{anthropic.__version__}")
        memory = None
        try:
            create = client.beta.memory_stores.memories.create
            arguments = {"path": "/matrix.md", "content": anthropic.__version__}
            if "view" in inspect.signature(create).parameters:
                arguments["view"] = "full"
            memory = create(store.id, **arguments)
            retrieved = client.beta.memory_stores.memories.retrieve(
                memory.id,
                memory_store_id=store.id,
            )
            assert retrieved.id == memory.id
            assert [item.id for item in client.beta.memory_stores.memories.list(store.id)] == [memory.id]
        finally:
            if memory is not None:
                client.beta.memory_stores.memories.delete(memory.id, memory_store_id=store.id)
            client.beta.memory_stores.archive(store.id)
            client.beta.memory_stores.delete(store.id)


def exercise_beta_ga_change_point(base_url: str, operation_ids: set[str]) -> None:
    if anthropic.__version__ not in {"0.121.0", "0.124.0"}:
        return
    # Projection decision table: 0.121 owns the last Beta-only Files/Skills
    # shape; 0.124 owns the first GA roots while retaining Beta. Create through
    # Beta, then retrieve/delete through GA iff that wheel exposes it. This
    # complements 1.2's full bidirectional handoff and pins both cutover edges.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=base_url, max_retries=0) as client:
        has_ga = "files.retrieve_metadata" in operation_ids
        file_root = client.files if has_ga else client.beta.files
        skill_root = client.skills if has_ga else client.beta.skills
        file = None
        skill = None
        try:
            file = client.beta.files.upload(file=("matrix.txt", b"matrix", "text/plain"))
            slug = anthropic.__version__.replace(".", "-")
            skill_body = (
                f"---\nname: python-matrix-{slug}\n"
                f"description: Python {anthropic.__version__} projection\n---\nFixture."
            ).encode()
            skill_arguments = {"files": [("SKILL.md", skill_body, "text/markdown")]}
            skill_create = client.beta.skills.create
            display = (
                "display_name"
                if "display_name" in inspect.signature(skill_create).parameters
                else "display_title"
            )
            skill_arguments[display] = f"Python Matrix {anthropic.__version__}"
            skill = skill_create(**skill_arguments)
            assert file_root.retrieve_metadata(file.id).id == file.id
            assert skill_root.retrieve(skill.id).id == skill.id
        finally:
            if skill is not None:
                skill_root.delete(skill.id)
            if file is not None:
                file_root.delete(file.id)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("version")
    parser.add_argument("base_url")
    args = parser.parse_args()
    assert anthropic.__version__ == args.version
    evidence, anchor = extracted_evidence(args.version)
    transport = importlib.import_module("httpx2" if args.version.startswith("1.") else "httpx")
    request_witnesses = exercise_declared_request_witnesses(
        anthropic,
        transport,
        evidence["operations"],
    )
    operation_ids = {operation["id"] for operation in evidence["operations"]}
    pathlike = exercise_pathlike_upload_change_point(
        anthropic,
        transport,
        operation_ids,
    )
    exercise_error_and_retry_contract(anthropic, transport)
    asyncio.run(exercise_async_error_and_retry_contract(anthropic, transport))
    exercise_library_exports(evidence["library_exports"])
    asyncio.run(exercise_resource_helper_surface(evidence["helpers"]))
    response_witnesses = exercise_current_response_compatibility(
        transport,
        evidence["operations"],
    )
    exercise_session(args.base_url, anchor["stream_event_names"])
    exercise_memory(args.base_url, operation_ids)
    exercise_beta_ga_change_point(args.base_url, operation_ids)
    print(
        f"PYTHON SDK MATRIX PASS {args.version}: {len(operation_ids)} sync/async operations, "
        f"{request_witnesses} declaration-derived request witnesses, "
        f"PathLike={pathlike}, "
        f"{response_witnesses} current-response witnesses per client mode, "
        f"{len(evidence['helpers'])} resource helpers, "
        f"{len(evidence['library_exports'])} library exports"
    )


if __name__ == "__main__":
    main()
