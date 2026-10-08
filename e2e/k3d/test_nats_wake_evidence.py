#!/usr/bin/env python3
"""Decision tables for the broker observation used by the real runner."""

import json
from pathlib import Path
import unittest
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts/ci"))
import _strict_unittest as strict_unittest

from nats_wake_evidence import qualifies, snapshot


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.base = dict(server_id="original", connections=1, subscriptions=1,
                         in_msgs=10, out_msgs=30)

    def test_wire_admission(self):
        # E1 object + exact identity + nonnegative integer counters => usable.
        # E2 each missing/malformed coordinate => unknown, never fabricated zero.
        # Vary one field at a time with every other precondition valid; bool is
        # tested explicitly because Python otherwise accepts it as an integer.
        self.assertEqual(snapshot(json.dumps(self.base)), self.base)
        for raw in ("", "{", "null", "[]", '"text"'):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                snapshot(raw)
        for key in self.base:
            invalid = [None, "", " ", 1] if key == "server_id" else [None, -1, "1", True, 1.5]
            for value in invalid:
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    snapshot(json.dumps({**self.base, key: value}))
            missing = {k: v for k, v in self.base.items() if k != key}
            with self.subTest(missing=key), self.assertRaises(ValueError):
                snapshot(json.dumps(missing))

    def test_readiness_and_progress_table(self):
        # C1 same broker; C2 one real Brain client connected; C3 active interest;
        # C4 received count grows; C5 sent count grows.
        # E3 readiness requires C2+C3; E4 progress requires all five, with an
        # admitted baseline. Enumerate all 32 combinations, not only single
        # failures. Counter reset/regression must not qualify as traffic.
        for mask in range(32):
            conditions = [bool(mask & (1 << bit)) for bit in range(5)]
            same, connected, interested, received, sent = conditions
            current = dict(server_id="original" if same else "replacement",
                           connections=1 if connected else 0,
                           subscriptions=1 if interested else 0,
                           in_msgs=11 if received else 10,
                           out_msgs=31 if sent else 30)
            with self.subTest(mask=mask):
                self.assertEqual(qualifies(current), connected and interested)
                self.assertEqual(qualifies(current, self.base), all(conditions))
        good = {**self.base, "in_msgs": 11, "out_msgs": 31}
        for key in ("in_msgs", "out_msgs"):
            self.assertFalse(qualifies({**good, key: 0}, self.base))
        for key in ("connections", "subscriptions"):
            self.assertFalse(qualifies(good, {**self.base, key: 0}))


if __name__ == "__main__":
    strict_unittest.main()
