#!/usr/bin/env python3
"""Perform the same prompt-free ACP handshake used by a production Worker."""

import asyncio
import json
import os
import signal
import shutil
import sys
import tempfile
import time
from pathlib import Path


async def read_response(process, request_id, runtime_id):
    while True:
        line = await process.stdout.readline()
        if not line:
            stderr = (await process.stderr.read()).decode(errors="replace")[-2000:]
            raise RuntimeError(f"{runtime_id}: ACP process closed early: {stderr}")
        try:
            message = json.loads(line)
        except json.JSONDecodeError as error:
            raise RuntimeError(
                f"{runtime_id}: ACP stdout was not JSON: {line.decode(errors='replace').strip()}"
            ) from error
        if message.get("id") == request_id and ("result" in message or "error" in message):
            if "error" in message:
                raise RuntimeError(f"{runtime_id}: ACP request {request_id} failed: {message['error']}")
            return message["result"]
        if message.get("id") is not None and message.get("method"):
            response = {
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32601, "message": "method not supported by capability probe"},
            }
            process.stdin.write((json.dumps(response, separators=(",", ":")) + "\n").encode())
            await process.stdin.drain()


async def request(process, request_id, method, params, runtime_id):
    message = {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": params,
    }
    process.stdin.write((json.dumps(message, separators=(",", ":")) + "\n").encode())
    await process.stdin.drain()
    return await read_response(process, request_id, runtime_id)


async def verify(runtime, root):
    runtime_id = runtime["id"]
    missing = [
        executable
        for executable in runtime["executables"]
        if shutil.which(executable) is None
    ]
    if missing:
        raise RuntimeError(
            f"{runtime_id}: required executables are unavailable: {', '.join(missing)}"
        )
    home = root / runtime_id
    workspace = home / "workspace"
    workspace.mkdir(parents=True)
    env = os.environ.copy()
    env.update({"HOME": str(home), "TMPDIR": str(home / "tmp")})
    (home / "tmp").mkdir()
    started = time.monotonic()
    process = await asyncio.create_subprocess_exec(
        *runtime["probe_argv"],
        cwd=workspace,
        env=env,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )

    async def negotiate():
        initialized = await request(
            process,
            1,
            "initialize",
            {"protocolVersion": 1, "clientCapabilities": {}},
            runtime_id,
        )
        print(f"ACP-RUNTIME PROBE: {runtime_id} initialized", file=sys.stderr, flush=True)
        auth_method_id = runtime.get("auth_method_id")
        if auth_method_id:
            advertised = {
                method.get("id") for method in initialized.get("authMethods", [])
            }
            if auth_method_id not in advertised:
                raise RuntimeError(
                    f"{runtime_id}: configured auth method {auth_method_id!r} was not advertised"
                )
            await request(
                process,
                5,
                "authenticate",
                {"methodId": auth_method_id},
                runtime_id,
            )
            print(
                f"ACP-RUNTIME PROBE: {runtime_id} authenticated",
                file=sys.stderr,
                flush=True,
            )
        session = await request(
            process,
            2,
            "session/new",
            {"cwd": str(workspace), "mcpServers": []},
            runtime_id,
        )
        if not session.get("sessionId"):
            raise RuntimeError(f"{runtime_id}: session/new returned no sessionId")
        print(f"ACP-RUNTIME PROBE: {runtime_id} session opened", file=sys.stderr, flush=True)
        return runtime_id, time.monotonic() - started

    try:
        try:
            result = await asyncio.wait_for(negotiate(), timeout=30)
        except asyncio.TimeoutError as error:
            if process.returncode is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    await asyncio.wait_for(process.wait(), timeout=2)
                except asyncio.TimeoutError:
                    os.killpg(process.pid, signal.SIGKILL)
                    await process.wait()
            stderr = (await process.stderr.read()).decode(errors="replace")[-2000:]
            raise RuntimeError(
                f"{runtime_id}: initialize+session/new exceeded the Worker probe deadline: "
                f"{stderr}"
            ) from error
    finally:
        if process.returncode is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                await asyncio.wait_for(process.wait(), timeout=2)
            except asyncio.TimeoutError:
                os.killpg(process.pid, signal.SIGKILL)
                await process.wait()
    return result


async def async_main():
    contract_path = Path("/usr/local/share/awaken/acp-runtimes.json")
    contract = json.loads(contract_path.read_text(encoding="utf-8"))
    requested = set(sys.argv[1:])
    runtimes = [
        runtime
        for runtime in contract["runtimes"]
        if not requested or runtime["id"] in requested
    ]
    known = {runtime["id"] for runtime in contract["runtimes"]}
    unknown = sorted(requested - known)
    if unknown:
        raise RuntimeError(f"unknown ACP runtime ids: {', '.join(unknown)}")
    if not runtimes:
        return
    with tempfile.TemporaryDirectory(prefix="awaken-acp-probe-") as directory:
        results = await asyncio.gather(
            *(verify(runtime, Path(directory)) for runtime in runtimes)
        )
    for runtime_id, elapsed in sorted(results):
        print(f"ACP-RUNTIME PASS: {runtime_id} initialize+session/new {elapsed:.2f}s")


if __name__ == "__main__":
    try:
        asyncio.run(async_main())
    except Exception as error:
        print(f"ACP-RUNTIME FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
