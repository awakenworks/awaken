"""Test-only proxy for the Coordinator's authenticated Control service calls."""

import hashlib
import http.client
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

lock = threading.Lock()
held = None
armed = False
deliver_path = "/internal/v1/control/webhooks/deliver"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def respond(self, status, body=b"", content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/healthz":
            self.respond(200)
        else:
            self.respond(404)

    def do_POST(self):
        global armed, held
        if self.path in ("/arm", "/held", "/release"):
            if self.client_address[0] != "127.0.0.1":
                self.respond(403)
                return
            with lock:
                if self.path == "/arm":
                    armed, held = True, None
                if self.path == "/release":
                    armed = False
                body = json.dumps(held).encode()
            self.respond(200, body)
            return

        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        upstream = http.client.HTTPConnection("control", 3001, timeout=10)
        try:
            headers = {k: v for k, v in self.headers.items() if k.lower() != "host"}
            upstream.request("POST", self.path, body, headers)
            response = upstream.getresponse()
            status = response.status
            response_body = response.read()
            content_type = response.getheader("Content-Type", "application/json")
        except (OSError, http.client.HTTPException):
            self.respond(502)
            return
        finally:
            upstream.close()

        if self.path == deliver_path and 200 <= status < 300:
            fact = json.loads(body)
            digest = hashlib.sha256(
                json.dumps(fact, sort_keys=True, separators=(",", ":")).encode()
            ).hexdigest()
            with lock:
                if armed and held is None and fact["event_type"] == "session.deleted":
                    held = fact["id"]
                withheld = armed and fact["id"] == held
                print(json.dumps({
                    "fact_id": fact["id"],
                    "object_id": fact["object_id"],
                    "event_type": fact["event_type"],
                    "payload_sha256": digest,
                    "coordinator_ip": self.client_address[0],
                    "withheld": withheld,
                }), flush=True)
            if withheld:
                self.respond(503, b'{"error":"test withheld the downstream receipt"}')
                return

        self.respond(status, response_body, content_type)


ThreadingHTTPServer(("0.0.0.0", 3001), Handler).serve_forever()
