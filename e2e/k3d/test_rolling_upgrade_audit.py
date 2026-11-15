import json
import tempfile
import unittest
from contextlib import redirect_stdout
from datetime import datetime, timedelta, timezone
from io import StringIO
from pathlib import Path

from rolling_upgrade_audit import attribution, durable_runs, receipts, retirement


def write_records(root, filename, rows):
    (root / filename).write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")


class RollingAuditTests(unittest.TestCase):
    def test_public_completion_cannot_hide_a_durable_cancellation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            accepted = {"kind": "accepted", "sessionId": "session-a", "receiptId": "evt-1", "marker": "input"}
            write_records(root, "traffic.jsonl", [accepted, {**accepted, "kind": "completed"}])
            committed = {"sessionId": "session-a", "marker": "input", "runId": "run-a"}
            write_records(root, "receipt-run-links.jsonl", [{**committed, "runState": {"Ended": "Cancelled"}}])
            self.assertFalse(durable_runs(root))
            write_records(root, "receipt-run-links.jsonl", [{**committed, "runState": {"Ended": "NaturalEnd"}}])
            self.assertTrue(durable_runs(root))

    def test_receipt_survival_remains_visible_when_the_outcome_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            key = {"sessionId": "session-a", "receiptId": "evt-1"}
            accepted = {"kind": "accepted", **key}
            survived = {"kind": "audited-receipts", "sessionId": "session-a", "count": 1}
            write_records(root, "traffic.jsonl", [accepted, survived, {"kind": "failure"}])
            with redirect_stdout(StringIO()) as output, self.assertRaises(AssertionError):
                receipts(root)
            self.assertIn("1 confirmed receipts survived rollback exactly once", output.getvalue())

            settled = {"kind": "completed", **key}
            audited = {"kind": "audited", "sessionId": "session-a", "count": 1}
            write_records(root, "traffic.jsonl", [accepted, survived, settled, audited])
            self.assertTrue(receipts(root))

    def test_failure_uses_the_exact_session_run_and_observed_image(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            error = {"kind": "terminal-error", "sessionId": "session-a", "receiptId": "event-a",
                     "marker": "ADR71-ROLLING-0-1", "stage": "mixed-coordinator",
                     "error": {"type": "model_request_failed_error", "message": "model admission failed: fixture"}}
            link = {"sessionId": "session-a", "marker": error["marker"], "runId": "run-a",
                    "runState": {"Ended": "Failed"}}
            observed = {"sessionId": "session-a", "runId": "run-a", "uid": "pod-a",
                        "image": "ghcr.io/owner/coordinator@sha256:" + "1" * 64, "image_id": "sha256:config-a"}
            wrong_run = {**observed, "runId": "run-b", "uid": "pod-b", "image": "wrong-image"}
            write_records(root, "traffic.jsonl", [error])
            write_records(root, "receipt-run-links.jsonl", [link])
            write_records(root, "model-admission-failures.jsonl", [wrong_run, observed])

            self.assertTrue(attribution(root))
            report = json.loads((root / "failure-attribution.json").read_text(encoding="utf-8"))[0]
            self.assertEqual(report["receiptId"], "event-a")
            self.assertEqual(report["coordinatorImage"], observed["image"])
            self.assertEqual(report["coordinatorImageId"], observed["image_id"])
            self.assertEqual(report["runStates"], {"run-a": {"Ended": "Failed"}})

            conflicting = {**observed, "uid": "pod-other", "image_id": "sha256:config-b"}
            write_records(root, "model-admission-failures.jsonl", [observed, conflicting])
            self.assertFalse(attribution(root))
            report = json.loads((root / "failure-attribution.json").read_text(encoding="utf-8"))[0]
            self.assertIsNone(report["coordinatorImage"])
            self.assertEqual(len(report["attempts"]), 2)

            unrelated = {**error, "error": {"type": "model_request_failed_error", "message": "provider failed"}}
            write_records(root, "traffic.jsonl", [unrelated])
            write_records(root, "model-admission-failures.jsonl", [observed])
            self.assertFalse(attribution(root))

    def test_retirement_pairs_each_uid_to_the_next_ready_worker_of_that_ordinal(self):
        start = datetime(2026, 1, 2, tzinfo=timezone.utc)

        def moment(seconds):
            return (start + timedelta(seconds=seconds)).isoformat()

        def pod(name, uid, image, ready, **changes):
            return {"name": name, "uid": uid, "role": "worker", "image": image, "image_id": "config-" + image,
                    "deleting": False, "deleted": False, "process_evidence_retained": True, "exit_at": None,
                    "ready_at": ready, **changes}

        events, drains = [], []
        for ordinal in range(2):
            name = f"worker-{ordinal}"
            for turn, (old, new) in enumerate([("maintenance", "candidate"), ("candidate", "maintenance")]):
                offset = (ordinal * 2 + turn) * 10
                uid, next_uid = f"{name}-{turn}", f"{name}-{turn + 1}"
                if turn == 0:
                    events.append(pod(name, uid, old, moment(offset)))
                drains.append({"uid": uid, "at": moment(offset + 1)})
                events.append(pod(name, uid, old, None, deleting=True, exit_at=moment(offset + 2)))
                events.append(pod(name, uid, old, None, deleting=True, exit_at=moment(offset + 2),
                                  deleted=True, observed_at=moment(offset + 4)))
                events.append(pod(name, next_uid, new, moment(offset + 7)))

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_records(root, "pod-events.jsonl", events)
            write_records(root, "drains.jsonl", drains)
            self.assertTrue(retirement(root, "maintenance", "candidate"))
            report = json.loads((root / "worker-retirements.json").read_text(encoding="utf-8"))
            self.assertEqual(len(report), 4)
            self.assertTrue(all(row["total_seconds"] == 6 for row in report))
            self.assertTrue(all(row["evidence_seconds"] == 2 for row in report))
