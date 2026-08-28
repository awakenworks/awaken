from __future__ import annotations

import argparse
import asyncio
import importlib
import inspect
import json
import os
import sys
import time
from pathlib import Path

import anthropic

from managed_python_sdk_request_contract import exercise_all_operation_requests


REPO = Path(__file__).resolve().parents[2]
ORACLE_PATH = Path(os.environ["AWAKEN_PYTHON_ORACLE"])
SCOPE_PATH = REPO / "packages/managed-sdk-oracle/config/scope.json"
sys.path.insert(0, str(REPO / "packages/managed-sdk-oracle/python"))
import oracle as oracle_generator  # noqa: E402 - exact repository extractor


def extracted_evidence(version: str) -> tuple[dict[str, object], dict[str, object]]:
    oracle = json.loads(ORACLE_PATH.read_text(encoding="utf-8"))
    anchor = next(item for item in oracle["anchors"] if item["version"] == version)
    installed_root = Path(anthropic.__file__).resolve().parent.parent
    scope = json.loads(SCOPE_PATH.read_text(encoding="utf-8"))
    evidence = oracle_generator.extract(installed_root, version, scope)
    direct = (
        "operation_fingerprint",
        "source_fingerprint",
        "source_file_count",
        "helper_fingerprint",
        "helpers",
        "library_export_fingerprint",
        "library_exports",
    )
    for field in direct:
        assert evidence[field] == anchor[field], f"{version}: installed {field}"
    assert len(evidence["operations"]) == anchor["operation_count"]
    assert len(evidence["library_exports"]) == anchor["library_export_count"]
    return evidence, anchor


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


def exercise_session(base_url: str) -> None:
    # Historical lifecycle graph: exact version defaults -> create -> exact
    # receipt -> limit=1 auto-pagination -> typed message+idle -> delete. Every
    # selected row is a reviewed source/protocol/transport change point; running
    # all rows therefore detects generated default and DTO regressions without
    # sampling redundant patch releases.
    with anthropic.Anthropic(api_key="e2e-dummy", base_url=base_url, max_retries=0) as client:
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
    evidence, _ = extracted_evidence(args.version)
    transport = importlib.import_module("httpx2" if args.version.startswith("1.") else "httpx")
    exercise_all_operation_requests(anthropic, transport, evidence["operations"])
    exercise_library_exports(evidence["library_exports"])
    asyncio.run(exercise_resource_helper_surface(evidence["helpers"]))
    operation_ids = {operation["id"] for operation in evidence["operations"]}
    exercise_session(args.base_url)
    exercise_memory(args.base_url, operation_ids)
    exercise_beta_ga_change_point(args.base_url, operation_ids)
    print(
        f"PYTHON SDK MATRIX PASS {args.version}: {len(operation_ids)} operations, "
        f"{len(evidence['helpers'])} resource helpers, {len(evidence['library_exports'])} library exports"
    )


if __name__ == "__main__":
    main()
