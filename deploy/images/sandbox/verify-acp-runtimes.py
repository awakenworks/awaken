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
import secrets
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen
from urllib.error import HTTPError
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


async def verify(runtime, root, timeout_seconds=30, mcp_servers=None, after_session=None):
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
            {"cwd": str(workspace), "mcpServers": mcp_servers or []},
            runtime_id,
        )
        if not session.get("sessionId"):
            raise RuntimeError(f"{runtime_id}: session/new returned no sessionId")
        print(f"ACP-RUNTIME PROBE: {runtime_id} session opened", file=sys.stderr, flush=True)
        if after_session is not None:
            await after_session()
        return runtime_id, time.monotonic() - started

    try:
        try:
            result = await asyncio.wait_for(negotiate(), timeout=timeout_seconds)
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


async def verify_private_mcp(runtime, root):
    # MCP cause/effect decision rules: M1 exact official HTTP Authorization
    # header reaches authenticated initialize/tools-list; M2 wrong/absent header
    # is refused; M3 after adapter shutdown no created file, config, database or
    # log contains the ephemeral credential marker. No model call is needed.
    token = secrets.token_hex(24)
    observed = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
            authenticated = self.headers.get("authorization") == "Bearer " + token
            observed.append((body.get("method"), authenticated))
            if not authenticated:
                self.send_response(401)
                self.end_headers()
                return
            if "id" not in body:
                self.send_response(202)
                self.end_headers()
                return
            result = {
                "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                "serverInfo": {"name": "private-auth-proof", "version": "1"},
            } if body.get("method") == "initialize" else {"tools": []}
            payload = json.dumps({"jsonrpc": "2.0", "id": body["id"], "result": result}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            self.send_response(405)
            self.end_headers()

        def do_DELETE(self):
            self.send_response(200)
            self.end_headers()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        for headers in ({}, {"Authorization": "Bearer incorrect"}):
            try:
                urlopen(Request(f"http://127.0.0.1:{server.server_port}/mcp",
                    data=b'{"jsonrpc":"2.0","id":0,"method":"initialize"}',
                    headers={"Content-Type": "application/json", **headers}), timeout=2)
            except HTTPError as error:
                if error.code != 401:
                    raise RuntimeError("MCP probe refused with an unexpected category") from error
            else:
                raise RuntimeError("MCP probe accepted an absent or wrong credential")
        async def await_authenticated_tools():
            # Session/new may precede async MCP startup. Keep the same owned
            # adapter and original deadline alive until the actual observation.
            while ("initialize", True) not in observed or ("tools/list", True) not in observed:
                await asyncio.sleep(0.1)

        result = await verify(runtime, root, mcp_servers=[{
            "type": "http", "name": "private-auth-proof",
            "url": f"http://127.0.0.1:{server.server_port}/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer " + token}],
        }], after_session=await_authenticated_tools)
        for path in root.rglob("*"):
            if path.is_file() and not path.is_symlink() and token.encode() in path.read_bytes():
                raise RuntimeError(f"{runtime['id']}: MCP credential persisted in created state")
        return result
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


async def self_test():
    # Probe-cleanup FMECA/cause-effect decision table:
    # C1=the adapter completes its handshake; C2=the adapter remains alive past
    # the deadline; C3=the adapter emits diagnostics. Effects: E1=return the
    # successful probe; E2=terminate the whole process group; E3=surface a
    # bounded diagnostic failure. V1 C1,!C2=>E1 is owned by production image
    # acceptance. V2 !C1,C2=>E2+E3 is exercised here with a real subprocess;
    # the short bound also detects a leaked child/process-wait path.
    runtime = {
        "id": "deadline-fixture",
        "executables": [sys.executable],
        "probe_argv": [
            sys.executable,
            "-c",
            "import sys,time; print('probe-stalled', file=sys.stderr, flush=True); time.sleep(30)",
        ],
        "auth_method_id": None,
    }
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="awaken-acp-verifier-test-") as directory:
        try:
            await verify(runtime, Path(directory), timeout_seconds=0.05)
        except RuntimeError as error:
            message = str(error)
            assert "exceeded the Worker probe deadline" in message, message
            assert "probe-stalled" in message, message
        else:
            raise AssertionError("V2: stalled adapter unexpectedly passed")
    assert time.monotonic() - started < 3, "V2: timed-out adapter was not reaped"

    # V3 Session/new completes but authenticated-MCP observation does not:
    # the same deadline must still kill/reap the owned adapter process group.
    runtime["probe_argv"] = [sys.executable, "-c", (
        "import sys,json; "
        "[(print(json.dumps({'jsonrpc':'2.0','id':json.loads(line)['id'],"
        "'result':{'sessionId':'opened'}}),flush=True)) for line in sys.stdin]"
    )]
    async def never_observed():
        await asyncio.sleep(30)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="awaken-acp-observation-test-") as directory:
        try:
            await verify(runtime, Path(directory), timeout_seconds=0.05,
                after_session=never_observed)
        except RuntimeError as error:
            assert "exceeded the Worker probe deadline" in str(error), str(error)
        else:
            raise AssertionError("V3: unobserved MCP unexpectedly passed")
    assert time.monotonic() - started < 3, "V3: observation wait leaked the adapter"


async def async_main():
    if sys.argv[1:] == ["--self-test"]:
        await self_test()
        return
    contract_path = Path("/usr/local/share/awaken/acp-runtimes.json")
    contract = json.loads(contract_path.read_text(encoding="utf-8"))
    authenticated_mcp = "--authenticated-mcp" in sys.argv[1:]
    requested = set(value for value in sys.argv[1:] if value != "--authenticated-mcp")
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
            *((verify_private_mcp if authenticated_mcp else verify)(runtime, Path(directory)) for runtime in runtimes)
        )
    for runtime_id, elapsed in sorted(results):
        print(f"ACP-RUNTIME PASS: {runtime_id} initialize+session/new {elapsed:.2f}s")


if __name__ == "__main__":
    try:
        asyncio.run(async_main())
    except Exception as error:
        print(f"ACP-RUNTIME FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
