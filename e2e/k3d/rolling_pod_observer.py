"""Collect pod and log evidence from the isolated rollout cluster."""

import argparse
import json
import re
import signal
import subprocess
import threading
from datetime import datetime, timezone
from pathlib import Path

FINALIZER = "awaken.dev/worker-process-evidence"
LOCK = threading.Lock()
STOPPING = threading.Event()
LOG_PROCESSES = {}
AUTHORITY_EVENTS = {
    "session_realization_lease_revoked",
    "attempt abandoned without a terminal commit; Dispatch recovery re-claims the Run",
    "dispatch lease renewal failed; retrying within safety window",
    "dispatch lease renewal timed out; retrying within safety window",
    "dispatch lease renewal could not prove ownership before safety deadline",
    "worker heartbeat transport failed; claim admission paused and retrying",
    "worker heartbeat request timed out; claim admission paused and retrying",
    "worker heartbeat explicitly lost authority; draining locally",
}
AUTHORITY_FIELDS = ("run_id", "session_id", "lease_epoch", "realization_epoch", "cause", "proof_remaining_ms")


def worker_authority_fields(message):
    # The Worker ships plain tracing output. Extract only the controlled event
    # and identity fields, keeping error details and request data out of the report.
    plain = re.sub(r"\x1b\[[0-9;]*m", "", message)
    for event in AUTHORITY_EVENTS:
        _, matched, suffix = plain.partition(event)
        if matched:
            identity = re.findall(r"(?:^|\s)(\w+)=([^=]+?)(?=\s+\w+=|$)", suffix)
            return {"event": event, **{key: value.strip().strip('"') for key, value in identity
                                      if key in AUTHORITY_FIELDS}}
    if plain.endswith("worker lost execution authority; supervisor restart required"):
        return {"event": "worker lost execution authority; supervisor restart required"}
    return None


def write(filename, record):
    with LOCK, (DIRECTORY / filename).open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(record) + "\n")


def follow_logs(pod):
    uid = pod["uid"]
    proc = subprocess.Popen(
        COMMAND + ["logs", pod["name"], "-c", pod["role"], "--timestamps", "--follow"],
        stdout=subprocess.PIPE, text=True,
    )
    with LOCK:
        LOG_PROCESSES[(uid, pod["container_id"])] = proc
        if STOPPING.is_set():
            proc.terminate()
    for line in proc.stdout:
        timestamp, _, message = line.rstrip().partition(" ")
        if pod["role"] == "worker":
            if message == "awaken-worker admin drain requested" or message.startswith(
                "awaken-worker draining: finishing in-flight runs"
            ):
                write("drains.jsonl", {"uid": uid, "at": timestamp})
            fields = worker_authority_fields(message)
            if fields:
                write("worker-authority.jsonl", {
                    "at": timestamp, "uid": uid, "pod": pod["name"], "container_id": pod["container_id"], **fields,
                })
            continue
        try:
            fields = json.loads(message).get("fields", {})
        except (json.JSONDecodeError, AttributeError):
            continue
        if fields.get("message") == "session_model_admission_failed":
            write("model-admission-failures.jsonl", {
                "at": timestamp, "uid": uid, "pod": pod["name"],
                "image": pod["image"], "image_id": pod["image_id"],
                "sessionId": fields["session_id"], "runId": fields["run_id"],
                "status": fields["status"],
            })
    proc.wait()


def watch_events(stream):
    decoder = json.JSONDecoder()
    data = ""
    for line in stream:
        data += line
        try:
            event, end = decoder.raw_decode(data.lstrip())
        except json.JSONDecodeError:
            continue
        assert not data.lstrip()[end:].strip()
        yield event
        data = ""
    if data.strip() and not STOPPING.is_set():
        raise ValueError("incomplete Kubernetes watch event")


def observe(watch):
    followers = set()
    previous = {}
    for event in watch_events(watch.stdout):
        obj = event["object"]
        if event["type"] == "ERROR":
            raise RuntimeError(obj)
        if event["type"] == "BOOKMARK":
            continue
        meta, status = obj["metadata"], obj.get("status", {})
        role = meta["labels"]["app"]
        container = next((c for c in status.get("containerStatuses", []) if c["name"] == role), {})
        state = container.get("state", {})
        ready = next((c["lastTransitionTime"] for c in status.get("conditions", [])
                      if c["type"] == "Ready" and c["status"] == "True"), None)
        pod = {
            "uid": meta["uid"], "name": meta["name"], "role": role,
            "image": next(c["image"] for c in obj["spec"]["containers"] if c["name"] == role),
            "image_id": container.get("imageID"),
            "container_id": container.get("containerID"),
            "restart_count": container.get("restartCount", 0),
            "ready_at": ready,
            "exit_at": state.get("terminated", {}).get("finishedAt"),
            "last_exit_at": container.get("lastState", {}).get("terminated", {}).get("finishedAt"),
            "deleting": bool(meta.get("deletionTimestamp")),
            "deleted": event["type"] == "DELETED",
            "process_evidence_retained": FINALIZER in meta.get("finalizers", []),
        }
        key = pod["uid"], pod["container_id"]
        if key not in followers and "running" in state and pod["image_id"] and pod["container_id"]:
            thread = threading.Thread(target=follow_logs, args=(pod,), daemon=True)
            followers.add(key)
            thread.start()
        if previous.get(pod["uid"]) != pod:
            previous[pod["uid"]] = pod
            write("pod-events.jsonl", {**pod, "observed_at": datetime.now(timezone.utc).isoformat()})
    if not STOPPING.is_set():
        raise RuntimeError(f"Kubernetes Pod watch ended with status {watch.wait()}")


def main():
    watch = subprocess.Popen(
        COMMAND + ["get", "pods", "-l", "app in (coordinator,worker)", "--watch",
                   "--output-watch-events", "-o", "json"],
        stdout=subprocess.PIPE, text=True,
    )

    def stop(*_):
        STOPPING.set()
        watch.terminate()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        observe(watch)
    finally:
        STOPPING.set()
        if watch.poll() is None:
            watch.terminate()
        watch.wait()
        with LOCK:
            processes = list(LOG_PROCESSES.values())
            for proc in processes:
                if proc.poll() is None:
                    proc.terminate()
        for proc in processes:
            proc.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("namespace")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    COMMAND = ["kubectl", "-n", args.namespace]
    DIRECTORY = args.directory
    main()
