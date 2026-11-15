"""Replay an observed claim from its own live Worker Pod over fixture TLS."""

import http.client
import json
import socket
import ssl
import sys
from pathlib import Path

coordinator_ip, path, serialized_claim, session_id = sys.argv[1:]
claim = json.loads(serialized_claim)
worker_id, generation, incarnation_id = claim["owner"].rsplit(":", 2)
directory = Path("/var/run/awaken/identity")
assert worker_id == "k8s:" + ":".join(
    (directory / key).read_text().strip() for key in ("namespace", "name", "uid")
)
identity = {
    "worker_id": worker_id,
    "incarnation_id": incarnation_id,
    "generation": int(generation),
}
body = {"claim": claim, "identity": identity}
if session_id:
    assert path == "/v1/worker/session/model-request/admit"
    body.update(session_id=session_id, thread_id=session_id, run_id=claim["run_id"])

context = ssl.create_default_context(cafile="/etc/awaken-worker-tls/ca.crt")
connection = http.client.HTTPSConnection("coordinator", 3443, context=context, timeout=10)
with socket.create_connection((coordinator_ip, 3443), timeout=10) as sock:
    with context.wrap_socket(sock, server_hostname="coordinator") as tls:
        connection.sock = tls
        connection.request("POST", path, json.dumps(body), {
            "Authorization": "Bearer " + (directory / "token").read_text().strip(),
            "Content-Type": "application/json",
            "X-Awaken-Worker-Id": worker_id,
            "X-Awaken-Worker-Incarnation": json.dumps(identity, separators=(",", ":")),
        })
        response = connection.getresponse()
        print(json.dumps({"status": response.status, "body": json.loads(response.read())}))
