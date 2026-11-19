"""Anthropic fixture for the split-role Session preview acceptance test."""

import argparse
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def frame(kind, **fields):
    return f"event: {kind}\ndata: {json.dumps(dict(type=kind, **fields))}\n\n".encode()


def chunks(marker):
    mode = marker.split("-", 3)[2]
    count = {"paced": 100, "broker": 100, "timeout": 100, "burst": 4_096, "load": 10}[mode]
    return [f"ADR71-PROVIDER:{marker}", *(f"[{index:04d}]" for index in range(count))]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    delayed = 0
    lock = threading.Lock()
    gates = threading.Condition()
    released = set()

    def await_release(self, marker, gate):
        with self.gates:
            ready = self.gates.wait_for(lambda: (marker, gate) in self.released, timeout=60)
            if ready:
                self.released.remove((marker, gate))
            return ready

    def send_json(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/readyz":
            self.send_json(200, {"ready": True})
        elif self.path == "/__preview/stats":
            with self.lock:
                self.send_json(200, {"delayed": self.delayed})
        else:
            self.send_json(404, {"error": "unknown fixture route"})

    def do_POST(self):
        release = re.fullmatch(r"/__preview/release/(ADR71-PREVIEW-(?:broker|timeout|burst)-[a-f0-9-]+)/([12])", self.path)
        if release:
            marker, gate = release.groups()
            with self.gates:
                self.released.add((marker, int(gate)))
                self.gates.notify_all()
            self.send_json(200, {"released": True})
            return
        if self.path == "/v1/worker/dispatch/stream_batch":
            if self.headers.get("Content-Length"):
                self.rfile.read(int(self.headers["Content-Length"]))
            with self.lock:
                type(self).delayed += 1
            # This route is a delayed network peer, with no Worker authority.
            time.sleep(2)
            try:
                self.send_json(503, {"error": "preview delay fixture"})
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if self.path != "/v1/messages" or self.headers.get("x-api-key") != "adr71-provider-secret":  # awaken-allow(secret):
            self.send_json(401, {"type": "error", "error": {"type": "authentication_error"}})
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        content = next(message["content"] for message in reversed(request["messages"]) if message["role"] == "user")
        marker = content if isinstance(content, str) else "".join(block.get("text", "") for block in content)
        if not re.fullmatch(r"ADR71-PREVIEW-(?:paced|broker|timeout|burst|load)-[a-f0-9-]+", marker):
            self.send_json(400, {"type": "error", "error": {"type": "invalid_request_error"}})
            return
        parts = chunks(marker)
        answer = {
            "id": "msg_preview_fixture", "type": "message", "role": "assistant",
            "model": request["model"], "content": [{"type": "text", "text": "".join(parts)}],
            "stop_reason": "end_turn", "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
        if not request.get("stream"):
            self.send_json(200, answer)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        try:
            self.wfile.write(frame("message_start", message={**answer, "content": [], "stop_reason": None}))
            self.wfile.write(frame("content_block_start", index=0, content_block={"type": "text", "text": ""}))
            mode = marker.split("-", 3)[2]
            terminal_gate = {"timeout": 72, "burst": len(parts) - 1}.get(mode)
            for index, part in enumerate(parts):
                if mode in ("broker", "timeout", "burst") and index == 4 and not self.await_release(marker, 1):
                    return
                if index == terminal_gate and not self.await_release(marker, 2):
                    return
                if mode in ("paced", "broker", "timeout"):
                    time.sleep(0.14)
                elif mode == "load":
                    time.sleep(0.03)
                elif index < 4:
                    time.sleep(0.08)
                self.wfile.write(frame("content_block_delta", index=0, delta={"type": "text_delta", "text": part}))
                self.wfile.flush()
            if mode == "burst":
                time.sleep(0.5)
            self.wfile.write(frame("content_block_stop", index=0))
            self.wfile.write(frame("message_delta", delta={"stop_reason": "end_turn", "stop_sequence": None}, usage={"output_tokens": 1}))
            self.wfile.write(frame("message_stop"))
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    argparse.ArgumentParser(description=__doc__, allow_abbrev=False).parse_args()
    ThreadingHTTPServer(("0.0.0.0", 3000), Handler).serve_forever()
