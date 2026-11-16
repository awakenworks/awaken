"""Evaluate the recorded traffic and receiver observations for one rollout."""

import argparse
import json
import re
import sys
from collections import Counter
from datetime import datetime
from pathlib import Path


def records(root, filename):
    data = (root / filename).read_text(encoding="utf-8")
    # The workload and the log collector can be writing while a stage is polled.
    return [json.loads(line) for line in data.splitlines(keepends=True) if line.endswith("\n")]


def receipt(record):
    return record["sessionId"], record["receiptId"]


def progress(root, stage):
    try:
        traffic = records(root, "traffic.jsonl")
        outbox = records(root, "outbox.jsonl")
    except FileNotFoundError:
        return False
    turns = {receipt(item) for item in traffic
             if item["kind"] == "completed" and item["stage"] == stage}
    errors = {receipt(item) for item in traffic
              if item["kind"] == "terminal-error" and item["stage"] == stage}
    sessions = {item["sessionId"] for item in traffic
                if item["kind"] == "lifecycle" and item["stage"] == stage}
    forwarded = {item["fact_id"] for item in outbox
                 if item["object_id"] in sessions and not item["withheld"]}
    if (turns or errors) and forwarded:
        print(f"{stage}: {len(turns)} completed turns, {len(errors)} terminal errors, "
              f"{len(forwarded)} acknowledged lifecycle facts")
        return True
    return False


def coordinator_ips(root, stage):
    rows = (root / f"pods-{stage}.tsv").read_text(encoding="utf-8").splitlines()
    return {cols[2] for row in rows if (cols := row.split("\t"))[0] == "coordinator"}


def replay(root, fact_id, before, after):
    prior = coordinator_ips(root, before)
    replacement = coordinator_ips(root, after)
    assert prior.isdisjoint(replacement), "observer source IP must identify the Coordinator version"
    deliveries = records(root, "outbox.jsonl")
    attempts = [item for item in deliveries if item["fact_id"] == fact_id]
    if not any(item["coordinator_ip"] in prior and item["withheld"] for item in attempts):
        return False
    if not any(item["coordinator_ip"] in replacement and not item["withheld"] for item in attempts):
        return False
    assert len({item["payload_sha256"] for item in attempts}) == 1, attempts
    assert {item["fact_id"] for item in deliveries
            if item["object_id"] == attempts[0]["object_id"]
            and item["event_type"] == "session.deleted"} == {fact_id}
    print(f"{before} → {after}: {len(attempts)} attempts with the same fact ID and payload {fact_id}")
    return True


def receipts(root):
    traffic = records(root, "traffic.jsonl")
    accepted = [receipt(item) for item in traffic if item["kind"] == "accepted"]
    readbacks = [item for item in traffic if item["kind"] == "audited-receipts"]
    expected_by_session = Counter(session for session, _ in accepted)
    observed_by_session = {item["sessionId"]: item["count"] for item in readbacks if item["count"]}
    assert accepted and len(accepted) == len(set(accepted))
    assert observed_by_session == expected_by_session, (observed_by_session, expected_by_session)
    print(f"{len(accepted)} confirmed receipts survived rollback exactly once")
    assert not any(item["kind"] == "failure" for item in traffic)
    completed = [receipt(item) for item in traffic if item["kind"] == "completed"]
    errors = [item for item in traffic if item["kind"] == "terminal-error"]
    settled = completed + [receipt(item) for item in errors]
    audited = sum(item["count"] for item in traffic if item["kind"] == "audited")
    assert accepted and len(accepted) == len(set(accepted)) == len(set(settled)) == len(settled) == audited
    assert set(accepted) == set(settled)
    print(f"{audited} confirmed receipts and outcomes verified in durable history after rollback")
    assert not errors, f"{len(errors)} turns reached a terminal error; see traffic.jsonl"
    return True


def event_posts(root):
    pattern = re.compile(r'"POST /v1/sessions/([^/? ]+)/events(?:\?[^ ]*)? HTTP/1\.[01]" (\d{3}) ')
    access_log = root / "nginx" / "access.log"
    lines = access_log.read_text(encoding="utf-8").splitlines() if access_log.exists() else []
    statuses, successful = Counter(), Counter()
    bad_gateways = []
    for line in lines:
        if match := pattern.search(line):
            session, status = match.groups()
            statuses[status] += 1
            if status == "200":
                successful[session] += 1
            if status == "502":
                bad_gateways.append(line)
    traffic = records(root, "traffic.jsonl") if (root / "traffic.jsonl").exists() else []
    accepted = [item for item in traffic if item["kind"] == "accepted"]
    missing = Counter(item["sessionId"] for item in accepted) - successful
    rollback_turns = sum(item["stage"] == "rolling-back-coordinator" for item in accepted)
    passed = bool(rollback_turns and not missing and not bad_gateways)
    report = {"passed": passed, "statuses": dict(statuses), "http502": bad_gateways,
              "coordinatorRollbackTurns": rollback_turns, "missingSuccessfulPosts": dict(missing)}
    (root / "event-posts.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"Event POST statuses {dict(statuses)}, Coordinator rollback turns {rollback_turns}; "
          "see event-posts.json")
    return passed


def receipt_run_states(root):
    links = {}
    for item in records(root, "receipt-run-links.jsonl"):
        links.setdefault((item["sessionId"], item["marker"]), {})[item["runId"]] = item.get("runState")
    return links


def durable_runs(root):
    accepted = [item for item in records(root, "traffic.jsonl") if item["kind"] == "accepted"]
    links = receipt_run_states(root)
    anomalies = []
    for item in accepted:
        runs = links.get((item["sessionId"], item["marker"]), {})
        if list(runs.values()) != [{"Ended": "NaturalEnd"}]:
            anomalies.append({**item, "runStates": runs})
    report = {"accepted": len(accepted), "naturalEnds": len(accepted) - len(anomalies),
              "anomalies": anomalies}
    (root / "durable-runs.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"{report['naturalEnds']}/{report['accepted']} accepted turns have one durable NaturalEnd")
    return bool(accepted) and not anomalies


def attribution(root):
    failures = [item for item in records(root, "traffic.jsonl")
                if item["kind"] in ("terminal-error", "observation-failure")]
    if not failures:
        (root / "failure-attribution.json").write_text("[]\n", encoding="utf-8")
        return True
    links = receipt_run_states(root)
    trace = root / "model-admission-failures.jsonl"
    attempts = records(root, trace.name) if trace.exists() else []
    result = []
    for failure in failures:
        runs = links.get((failure["sessionId"], failure["marker"]), {})
        error = failure.get("error") or {}
        admission_failed = error.get("type") == "model_request_failed_error" and error.get(
            "message", ""
        ).startswith("model admission failed:")
        matches = [item for item in attempts if admission_failed and len(runs) == 1
                   and item["sessionId"] == failure["sessionId"] and item["runId"] in runs]
        digests = {(item["image"], item["image_id"]) for item in matches}
        image, image_id = next(iter(digests)) if len(digests) == 1 else (None, None)
        result.append({
            "sessionId": failure["sessionId"], "receiptId": failure["receiptId"],
            "kind": failure["kind"], "stage": failure["stage"], "runIds": sorted(runs), "runStates": runs,
            "coordinatorImage": image, "coordinatorImageId": image_id,
            "attempts": matches,
        })
    (root / "failure-attribution.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    attributed = sum(item["coordinatorImage"] is not None for item in result)
    print(f"{attributed}/{len(failures)} failed receipts linked to one Coordinator image")
    return attributed == len(failures)


def verify_model_trace(root, claim, session_id, image_id):
    try:
        attempts = records(root, "model-admission-failures.jsonl")
    except FileNotFoundError:
        return False
    run_id = json.loads(claim)["run_id"]
    return any(item["sessionId"] == session_id and item["runId"] == run_id and item["image_id"] == image_id
               for item in attempts)


def retirement(root, legacy, candidate):
    events = [item for item in records(root, "pod-events.jsonl") if item["role"] == "worker"]
    drains = {}
    for item in records(root, "drains.jsonl"):
        drains.setdefault(item["uid"], item["at"])
    by_uid = {}
    for event in events:
        by_uid.setdefault(event["uid"], []).append(event)
    measurements = []
    for uid, states in by_uid.items():
        if not any(event["deleting"] for event in states):
            continue
        pod = states[0]
        assert pod["image"] in (legacy, candidate), pod
        assert any(event["process_evidence_retained"] for event in states), pod
        exit_at = next((event["exit_at"] for event in states if event["deleting"] and event["exit_at"]), None)
        # Kubernetes can send only DELETED, with the last retained object as
        # payload. The deletion event itself proves this exact UID was released.
        retired = next((event for event in states
                        if event["deleting"] and (event["deleted"] or not event["process_evidence_retained"])), None)
        assert retired, f"missing process evidence release for {uid}"
        replacement = next(event for event in events[events.index(retired) + 1:]
                           if event["name"] == pod["name"] and event["uid"] != uid and event["ready_at"])
        target = candidate if pod["image"] == legacy else legacy
        assert replacement["image"] == target and replacement["image_id"], replacement
        assert exit_at and uid in drains, f"missing drain or exact process exit for {uid}"
        timestamps = [drains[uid], exit_at, retired["observed_at"], replacement["ready_at"]]
        times = [datetime.fromisoformat(value.replace("Z", "+00:00")) for value in timestamps]
        intervals = [(later - earlier).total_seconds() for earlier, later in zip(times, times[1:])]
        # Kubernetes reports process exit and readiness at whole-second precision.
        assert all(value >= -1 for value in intervals), timestamps
        measurements.append({
            "worker": pod["name"], "uid": uid, "image": pod["image"], "replacement_uid": replacement["uid"],
            "replacement_image": target, "drain_at": timestamps[0], "exited_at": timestamps[1],
            "retirement_observed_at": timestamps[2], "replacement_ready_at": timestamps[3],
            "drain_seconds": max(0, intervals[0]), "evidence_seconds": max(0, intervals[1]),
            "replacement_seconds": max(0, intervals[2]),
            "total_seconds": (times[-1] - times[0]).total_seconds(),
        })
    (root / "worker-retirements.json").write_text(json.dumps(measurements, indent=2) + "\n", encoding="utf-8")
    retired = [(item["worker"], item["image"]) for item in measurements]
    assert sorted(retired) == sorted((name, image) for name in ("worker-0", "worker-1")
                                    for image in (legacy, candidate)), retired
    print("Observed four Worker retirements and their replacement readiness")
    return True


if __name__ == "__main__":
    parser = argparse.ArgumentParser(allow_abbrev=False)
    commands = parser.add_subparsers(required=True)
    for action, (check, names) in {
        "progress": (progress, ("stage",)),
        "replay": (replay, ("fact_id", "before", "after")),
        "receipts": (receipts, ()),
        "event-posts": (event_posts, ()),
        "durable-runs": (durable_runs, ()),
        "attribution": (attribution, ()),
        "retirement": (retirement, ("legacy", "candidate")),
        "verify-model-trace": (verify_model_trace, ("claim", "session_id", "image_id")),
    }.items():
        command = commands.add_parser(action, allow_abbrev=False)
        command.add_argument("root", type=Path)
        for name in names:
            command.add_argument(name)
        command.set_defaults(check=check)
    args = vars(parser.parse_args())
    sys.exit(0 if args.pop("check")(**args) else 1)
