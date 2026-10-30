"""T30-C02 Python half: the official `anthropic` SDK (locked 1.12.1) runs the
same Environment/Memory/Session lifecycle, pagination and error rows as the
TypeScript half against ONE production Awaken server, and prints one JSON
observation on stdout. Driven by sdk_differential_e2e.mjs.

Usage: python sdk_differential_driver.py lifecycle <label> <model>
       python sdk_differential_driver.py observe <store_id>
"""
from __future__ import annotations

import json
import os
import sys

def error_row(call):
    try:
        call()
    except anthropic.APIStatusError as error:
        body = error.body if isinstance(error.body, dict) else {}
        return {
            "class": type(error).__name__,
            "status": error.status_code,
            "type": (body.get("error") or {}).get("type"),
            "request_id": bool(error.request_id and error.request_id.startswith("req_")),
        }
    return {"class": None}


def lifecycle(client: anthropic.Anthropic, label: str, model: str) -> dict:
    beta = client.beta
    request_ids = []

    environment = beta.environments.create(
        name=f"t30-{label}-env", config={"type": "self_hosted"}
    )
    request_ids.append(environment._request_id)
    retrieved = beta.environments.retrieve(environment.id)
    updated = beta.environments.update(environment.id, description="updated by the lifecycle")
    archived = beta.environments.archive(environment.id)

    store = beta.memory_stores.create(name=f"t30-{label}-store")
    request_ids.append(store._request_id)
    memories = []
    for index in range(3):
        memory = beta.memory_stores.memories.create(
            store.id, path=f"/p{index}.md", content=f"c{index}"
        )
        request_ids.append(memory._request_id)
        memories.append(memory)
    beta.memory_stores.memories.update(
        memories[0].id,
        memory_store_id=store.id,
        content="c0-v2",
        precondition={"type": "content_sha256", "content_sha256": memories[0].content_sha256},
    )
    stale = error_row(
        lambda: beta.memory_stores.memories.update(
            memories[0].id,
            memory_store_id=store.id,
            content="stale",
            precondition={"type": "content_sha256", "content_sha256": memories[0].content_sha256},
        )
    )
    versions = list(beta.memory_stores.memory_versions.list(store.id, limit=2))
    listed = list(beta.memory_stores.memories.list(store.id, limit=2))
    ordinal = {memory.id: index for index, memory in enumerate(memories)}

    agent = beta.agents.create(name=f"t30-{label}-agent", model=model, system="t30 differential")
    request_ids.append(agent._request_id)
    session = beta.sessions.create(agent=agent.id, environment_id="env_local")
    request_ids.append(session._request_id)
    same_session = beta.sessions.retrieve(session.id).id == session.id
    beta.sessions.delete(session.id)

    return {
        "normalized": {
            "environment": {
                "retrieved_same": retrieved.id == environment.id,
                "description": updated.description,
                "archived": archived.archived_at is not None,
            },
            "memory": {
                "versions": [[v.operation, v.path, ordinal.get(v.memory_id)] for v in versions],
                "listed_paths": [m.path for m in listed],
                "stale_update": stale,
            },
            "session": {
                "retrieved_same": same_session,
                "status": session.status,
                "after_delete": error_row(lambda: beta.sessions.retrieve(session.id)),
            },
            "errors": {
                "missing_environment": error_row(
                    lambda: beta.environments.retrieve("env_t30_does_not_exist")
                ),
                "zero_page_limit": error_row(lambda: beta.sessions.list(limit=0)),
            },
        },
        "ids": {"store": store.id, "environment": environment.id, "agent": agent.id},
        "request_ids": request_ids,
    }


def observe(client: anthropic.Anthropic, store_id: str) -> dict:
    beta = client.beta
    store = beta.memory_stores.retrieve(store_id)
    return {
        "store": [store.id, store.name],
        "memories": [
            [m.id, m.path, m.content_sha256]
            for m in beta.memory_stores.memories.list(store_id, limit=2)
        ],
        "versions": [
            [v.id, v.operation, v.memory_id, v.path]
            for v in beta.memory_stores.memory_versions.list(store_id, limit=2)
        ],
    }


USAGE = "usage: sdk_differential_driver.py lifecycle <label> <model> | observe <store_id>"


def main() -> None:
    arity = {"lifecycle": 4, "observe": 3}
    if len(sys.argv) < 2 or arity.get(sys.argv[1]) != len(sys.argv):
        raise SystemExit(USAGE)
    # Imported after the argument check, so a usage error (and the gate's
    # argument probe) does not depend on the SDK being installed.
    global anthropic
    import anthropic

    mode = sys.argv[1]
    with anthropic.Anthropic(api_key="e2e-local", base_url=os.environ["AWAKEN_MANAGED_BASE_URL"], max_retries=0) as client:  # awaken-allow(secret): local test key
        if mode == "lifecycle":
            result = lifecycle(client, sys.argv[2], sys.argv[3])
        elif mode == "observe":
            result = observe(client, sys.argv[2])
    result["sdk"] = {"language": "python", "version": anthropic.__version__}
    print(json.dumps(result))


if __name__ == "__main__":
    main()
