#!/usr/bin/env python3
"""Exercise already-admitted Pod Create across three real API race outcomes.

This is an opt-in k3d negative control for ADR-0078, not a cleanup oracle.
It creates only a uniquely named Namespace, Pod, and validating webhook for
each invocation, then deletes and verifies those exact resources.
"""

from __future__ import annotations

import argparse
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
from pathlib import Path
import secrets
import ssl
import subprocess
import tempfile
import threading


def command(argv: list[str], *, body: dict | None = None, timeout: int = 20) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        input=None if body is None else json.dumps(body),
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--kubeconfig", type=Path, required=True)
    parser.add_argument("--advertise-address", required=True)
    parser.add_argument(
        "--scenario",
        choices=("namespace-deletion", "reservation-held", "reservation-released"),
        default="namespace-deletion",
    )
    args = parser.parse_args()
    kubeconfig = args.kubeconfig.resolve(strict=True)
    address = str(ipaddress.IPv4Address(args.advertise_address))
    base = ["kubectl", "--kubeconfig", str(kubeconfig)]
    context = command([*base, "config", "current-context"])
    if context.returncode or not context.stdout.strip().startswith("k3d-"):
        raise SystemExit("negative control requires an explicit k3d context")
    base.extend(["--context", context.stdout.strip()])
    for resource in ("namespaces", "validatingwebhookconfigurations.admissionregistration.k8s.io"):
        allowed = command([*base, "auth", "can-i", "create", resource])
        if allowed.returncode or allowed.stdout.strip() != "yes":
            raise SystemExit(f"negative control cannot create {resource}")

    nonce = secrets.token_hex(5)
    name = f"awaken-late-create-{nonce}"
    pod_name = "late-create"
    label = "awaken.dev/late-create-negative-control"
    reached = threading.Event()
    release = threading.Event()

    class Admission(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            request = json.loads(self.rfile.read(length))
            uid = request["request"]["uid"]
            reached.set()
            allowed = release.wait(25)
            payload = json.dumps({
                "apiVersion": "admission.k8s.io/v1",
                "kind": "AdmissionReview",
                "response": {"uid": uid, "allowed": allowed},
            }).encode()
            try:
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *_arguments: object) -> None:
            pass

    # Cause/effect rule N1: the apiserver admitted Pod Create and pauses in the
    # webhook; the Namespace then fully disappears; releasing that same request
    # can still create the Pod. Effect: Namespace finalization/404 is not a
    # cross-process request-closure witness. N2 (Create starts only after
    # deletion) is the ordinary NamespaceLifecycle rejection and proves less;
    # the real fault cut is N1's already-admitted request.
    with tempfile.TemporaryDirectory(prefix="awaken-late-create-") as temporary:
        root = Path(temporary)
        certificate = root / "cert.pem"
        key = root / "key.pem"
        generated = command([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(key), "-out", str(certificate), "-days", "1",
            "-subj", f"/CN={name}", "-addext", f"subjectAltName=IP:{address}",
        ])
        if generated.returncode:
            raise RuntimeError("generate isolated webhook TLS certificate failed")
        server = ThreadingHTTPServer(("0.0.0.0", 0), Admission)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(str(certificate), str(key))
        server.socket = tls.wrap_socket(server.socket, server_side=True)
        serving = threading.Thread(target=server.serve_forever, daemon=True)
        serving.start()

        namespace = {
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": {"name": name, "labels": {label: nonce}},
        }
        webhook = {
            "apiVersion": "admissionregistration.k8s.io/v1",
            "kind": "ValidatingWebhookConfiguration",
            "metadata": {"name": name},
            "webhooks": [{
                "name": f"late-create-{nonce}.awaken.dev",
                "admissionReviewVersions": ["v1"],
                "sideEffects": "None",
                "failurePolicy": "Fail",
                "matchPolicy": "Exact",
                "timeoutSeconds": 30,
                "namespaceSelector": {"matchLabels": {label: nonce}},
                # Only the original Pod is held. The provider-owned inert
                # reservation must race the paused request, not this webhook.
                "objectSelector": {"matchLabels": {label: nonce}},
                "rules": [{
                    "apiGroups": [""], "apiVersions": ["v1"],
                    "operations": ["CREATE"], "resources": ["pods"],
                    "scope": "Namespaced",
                }],
                "clientConfig": {
                    "url": f"https://{address}:{server.server_port}/validate",
                    "caBundle": base64.b64encode(certificate.read_bytes()).decode(),
                },
            }],
        }
        pod = {
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"namespace": name, "name": pod_name, "labels": {label: nonce}},
            "spec": {"restartPolicy": "Never", "containers": [{
                "name": "sleep", "image": "busybox:1.36",
                "command": ["sleep", "120"],
            }]},
        }
        creating: subprocess.Popen[str] | None = None
        try:
            result = command([*base, "apply", "-f", "-"], body=namespace)
            if result.returncode:
                raise RuntimeError(f"create isolated Namespace failed: {result.stderr.strip()}")
            result = command([*base, "apply", "-f", "-"], body=webhook)
            if result.returncode:
                raise RuntimeError(f"create isolated webhook failed: {result.stderr.strip()}")
            creating = subprocess.Popen(
                [*base, "create", "-f", "-", "--request-timeout=35s"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True,
            )
            assert creating.stdin is not None
            creating.stdin.write(json.dumps(pod))
            creating.stdin.close()
            creating.stdin = None
            if not reached.wait(10):
                raise RuntimeError("Pod Create did not reach the selected admission cut")
            if args.scenario == "namespace-deletion":
                deleted = command([*base, "delete", "namespace", name, "--wait=true", "--timeout=15s"], timeout=20)
                if deleted.returncode:
                    raise RuntimeError(f"Namespace did not finalize: {deleted.stderr.strip()}")
                absent = command([*base, "get", "namespace", name, "-o", "name"])
                if absent.returncode == 0:
                    raise RuntimeError("Namespace still exists before releasing admitted Create")
                release.set()
                stdout, stderr = creating.communicate(timeout=10)
                if creating.returncode:
                    raise RuntimeError(f"already-admitted Pod Create failed: {stderr.strip()}")
                observed = command([*base, "get", "pods", "-A", "--field-selector", f"metadata.name={pod_name}", "-o", "json"])
                if observed.returncode:
                    raise RuntimeError(f"late Pod observation failed: {observed.stderr.strip()}")
                items = json.loads(observed.stdout)["items"]
                if not any(item["metadata"]["namespace"] == name and item["metadata"]["name"] == pod_name for item in items):
                    raise RuntimeError("late Pod was not observable after Namespace finalization")
                print(f"N1 proved: {stdout.strip()} after Namespace {name} finalized")
            else:
                # C1 the original Pod POST is already paused inside admission;
                # C2 a nonrunning Pod reserves the exact namespace/name; C3 the
                # reservation remains or is deleted before C1 resumes.
                # E1 C1+C2+held -> old POST 409, same reservation UID retained;
                # E2 C1+C2+released -> old POST 201 under a NEW UID. Rules
                # R1=C1+C2+held->E1, R2=C1+C2+released->E2. Neither a
                # Namespace 404 nor a reservation that was once present is a
                # durable closure witness; the current exact name matters.
                reservation = {
                    "apiVersion": "v1", "kind": "Pod",
                    "metadata": {
                        "namespace": name, "name": pod_name,
                        "labels": {"awaken.dev/late-create-reservation-control": nonce},
                    },
                    "spec": {
                        "automountServiceAccountToken": False,
                        "restartPolicy": "Never",
                        "schedulingGates": [{"name": "awaken.dev/create-isolation"}],
                        "nodeSelector": {"awaken.dev/create-isolation-node": "never"},
                        "containers": [{
                            "name": "inert", "image": "invalid.invalid/awaken-create-reservation:never",
                            "imagePullPolicy": "Never", "command": ["/bin/false"],
                        }],
                    },
                }
                reserved = command([*base, "create", "-f", "-", "-o", "json"], body=reservation)
                if reserved.returncode:
                    raise RuntimeError(f"same-name reservation failed: {reserved.stderr.strip()}")
                reservation_uid = json.loads(reserved.stdout)["metadata"]["uid"]

                def exact_pod() -> dict:
                    result = command([*base, "get", "pod", pod_name, "-n", name, "-o", "json"])
                    if result.returncode:
                        raise RuntimeError(f"exact Pod read failed: {result.stderr.strip()}")
                    return json.loads(result.stdout)

                observed = exact_pod()
                if (observed["metadata"]["uid"] != reservation_uid
                        or observed["metadata"].get("deletionTimestamp")
                        or observed["spec"].get("nodeName")
                        or observed["metadata"]["labels"].get("awaken.dev/late-create-reservation-control") != nonce):
                    raise RuntimeError("reservation lost its exact inert UID before the race")
                if args.scenario == "reservation-released":
                    deleted = command([*base, "delete", "pod", pod_name, "-n", name,
                                       "--wait=true", "--timeout=15s"], timeout=20)
                    if deleted.returncode:
                        raise RuntimeError(f"reservation could not be released: {deleted.stderr.strip()}")
                    absent = command([*base, "get", "pod", pod_name, "-n", name, "-o", "name"])
                    if absent.returncode == 0:
                        raise RuntimeError("reservation name remained occupied before old Create resumed")
                release.set()
                stdout, stderr = creating.communicate(timeout=10)
                if args.scenario == "reservation-held":
                    if creating.returncode == 0 or "AlreadyExists" not in stderr:
                        raise RuntimeError(f"old Create did not lose the held name: {stdout.strip()} {stderr.strip()}")
                    if exact_pod()["metadata"]["uid"] != reservation_uid:
                        raise RuntimeError("held reservation UID changed after old Create 409")
                    print(f"R1 proved: old Create returned 409; reservation UID {reservation_uid} retained")
                else:
                    if creating.returncode:
                        raise RuntimeError(f"old Create did not publish after name release: {stderr.strip()}")
                    published = exact_pod()
                    if (published["metadata"]["uid"] == reservation_uid
                            or published["metadata"]["labels"].get(label) != nonce):
                        raise RuntimeError("released name did not publish the exact old Pod under a new UID")
                    print(f"R2 proved: released reservation UID {reservation_uid}; old Create published UID {published['metadata']['uid']}")
        finally:
            release.set()
            cleanup_errors = []
            if creating is not None and creating.poll() is None:
                try:
                    creating.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    creating.kill()
                    creating.wait(timeout=5)
                    cleanup_errors.append("admitted Create outcome Unknown")
            command([*base, "delete", "validatingwebhookconfiguration", name,
                     "--ignore-not-found", "--wait=true", "--timeout=15s"])
            command([*base, "delete", "pod", pod_name, "-n", name,
                     "--ignore-not-found", "--wait=true", "--timeout=15s"])
            command([*base, "delete", "namespace", name,
                     "--ignore-not-found", "--wait=true", "--timeout=15s"])
            server.shutdown()
            server.server_close()
            serving.join(timeout=5)
            remaining = command([*base, "get", "pods", "-A", "--field-selector", f"metadata.name={pod_name}", "-o", "json"])
            if remaining.returncode != 0 or any(
                item["metadata"]["namespace"] == name
                for item in json.loads(remaining.stdout)["items"]
            ):
                cleanup_errors.append("late Pod status unresolved")
            for kind in ("namespace", "validatingwebhookconfiguration"):
                state = command([*base, "get", kind, name, "-o", "name"])
                if state.returncode == 0:
                    cleanup_errors.append(f"{kind} remained")
            if cleanup_errors:
                raise RuntimeError(f"exact negative-control cleanup failed: {', '.join(cleanup_errors)}")
            print(f"cleanup verified: {name} Pod, Namespace and webhook absent")


if __name__ == "__main__":
    main()
