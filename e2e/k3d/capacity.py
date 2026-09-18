#!/usr/bin/env python3
"""Capacity admission for the existing local-Docker k3d test harness.

Budgets are conservative fixture estimates, not a guarantee against concurrent
disk use: import/ENOSPC and cleanup errors still have to fail the actual test.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

GIB = 1024 ** 3
MIB = 1024 ** 2
_BINARY_QUANTITY = re.compile(r"^([1-9][0-9]*)(Ki|Mi|Gi|Ti)$")
_BINARY_UNITS = {"Ki": 1024, "Mi": MIB, "Gi": GIB, "Ti": 1024 ** 4}


def parse_binary_quantity(value: str) -> int:
    match = _BINARY_QUANTITY.fullmatch(value)
    if match is None:
        raise ValueError(f"invalid absolute capacity quantity: {value}")
    return int(match.group(1)) * _BINARY_UNITS[match.group(2)]


def filesystem(path: str) -> tuple:
    stats = os.statvfs(path)
    return os.stat(path).st_dev, stats.f_bavail * stats.f_frsize, stats.f_favail


def capacity_plan(nodes: int, image_sizes: list[int], docker_root: str,
                  temporary_root: str, observe=filesystem,
                  eviction_threshold_bytes: int | None = None) -> list[dict]:
    if type(nodes) is not int or nodes < 1:
        raise ValueError("node count must be a positive integer")
    if any(type(size) is not int or size < 0 for size in image_sizes):
        raise ValueError("image sizes must be nonnegative integer bytes")
    if (eviction_threshold_bytes is not None
            and (type(eviction_threshold_bytes) is not int or eviction_threshold_bytes <= 0)):
        raise ValueError("eviction threshold must be positive integer bytes")
    total = sum(image_sizes)
    # All nodes import images. Count both content and unpacked snapshots per
    # node, a transport archive copy, 2 GiB/node for bootstrap/WAL, and an 8 GiB
    # host reserve. An explicit kubelet threshold must retain both its hard
    # trigger and minimum-reclaim margin after import; otherwise admission can
    # leave a Ready node tainted with DiskPressure. Tar framing uses 25% plus
    # 64 MiB per image. Image-inode budgets are a fixture floor; exhaustion
    # during import remains fatal.
    archive = (total * 5 + 3) // 4 + len(image_sizes) * 64 * MIB
    post_import_reserve = max(
        8 * GIB,
        2 * eviction_threshold_bytes if eviction_threshold_bytes is not None else 0,
    )
    allocations = (
        (docker_root, 2 * nodes * total + archive + 2 * nodes * GIB + post_import_reserve,
         (nodes + 1) * 100_000),
        (temporary_root, archive, len(image_sizes) + 32),
    )
    devices = {}
    for path, needed, needed_inodes in allocations:
        device, available, inodes = observe(path)
        if available < 0 or inodes < 0:
            raise ValueError(f"unobservable capacity: {path}")
        entry = devices.setdefault(device, {
            "paths": [], "required_bytes": 0, "available_bytes": available,
            "required_inodes": 0, "available_inodes": inodes,
        })
        entry["paths"].append(path)
        entry["required_bytes"] += needed
        entry["required_inodes"] += needed_inodes
        entry["available_bytes"] = min(entry["available_bytes"], available)
        entry["available_inodes"] = min(entry["available_inodes"], inodes)
    return list(devices.values())


def require_capacity(plan: list[dict]) -> None:
    for entry in plan:
        if (entry["available_bytes"] < entry["required_bytes"]
                or entry["available_inodes"] < entry["required_inodes"]):
            raise ValueError("insufficient k3d capacity: " + json.dumps(entry, sort_keys=True))


def docker_output(*args: str) -> str:
    return subprocess.check_output(["docker", *args], text=True).strip()


def inspect_local_capacity(nodes: int, images: list[str],
                           eviction_threshold_bytes: int | None = None) -> list[dict]:
    # A remote daemon's /data/docker is not the client's filesystem. Reject
    # remote contexts until a daemon-side capacity observer is provided.
    endpoint = (os.environ.get("DOCKER_HOST") if not os.environ.get("DOCKER_CONTEXT") else None)
    endpoint = endpoint or docker_output("context", "inspect", "--format", "{{.Endpoints.docker.Host}}")
    if not endpoint.startswith("unix://"):
        raise ValueError("k3d capacity admission requires a local Unix-socket Docker context")
    root = docker_output("info", "--format", "{{.DockerRootDir}}")
    if not Path(root).is_absolute():
        raise ValueError("Docker root must be an observable absolute directory")
    records = json.loads(docker_output("image", "inspect", *images)) if images else []
    # Tags may alias the same immutable image. Shared layers across different
    # images are deliberately overcounted instead of assuming node cache hits.
    sizes = {record["Id"]: record["Size"] for record in records}
    return capacity_plan(
        nodes,
        list(sizes.values()),
        root,
        tempfile.gettempdir(),
        eviction_threshold_bytes=eviction_threshold_bytes,
    )


class CapacityTests(unittest.TestCase):
    def test_mount_identity_controls_accumulation(self):
        # K1/K2: C1 Docker/tmp share a device or not; C2 four nodes and one
        # 1 GiB image. Shared mounts must sum both simultaneous allocations.
        separate = capacity_plan(4, [GIB], "/docker", "/tmp",
                                 lambda path: (path, 100 * GIB, 1_000_000))
        self.assertEqual([r["required_bytes"] for r in separate], [27_179_089_920, 1_409_286_144])
        shared = capacity_plan(4, [GIB], "/docker", "/tmp",
                               lambda path: (1, 100 * GIB, 1_000_000))
        self.assertEqual(len(shared), 1)
        self.assertEqual(shared[0]["required_bytes"], 28_588_376_064)
        self.assertEqual(shared[0]["required_inodes"], 500_033)

    def test_each_capacity_axis_and_exact_boundary(self):
        # C1 byte budget met; C2 inode budget met. Only C1+C2 admits. Test
        # required-1, exact required, required+1 independently for both axes.
        for byte_delta in (-1, 0, 1):
            for inode_delta in (-1, 0, 1):
                row = {"paths": ["fixture"], "required_bytes": 100,
                       "available_bytes": 100 + byte_delta, "required_inodes": 50,
                       "available_inodes": 50 + inode_delta}
                with self.subTest(byte_delta=byte_delta, inode_delta=inode_delta):
                    if byte_delta < 0 or inode_delta < 0:
                        with self.assertRaises(ValueError):
                            require_capacity([row])
                    else:
                        require_capacity([row])

    def test_eviction_threshold_retains_the_complete_recovery_target(self):
        # C1 default fixture policy keeps the existing 8 GiB reserve; C2 an
        # 8 GiB hard threshold with an equal minimum reclaim keeps 16 GiB after
        # import; C3 a smaller threshold cannot weaken the baseline reserve.
        observe = lambda path: (path, 100 * GIB, 1_000_000)
        default = capacity_plan(1, [], "/docker", "/tmp", observe)
        large = capacity_plan(
            1, [], "/docker", "/tmp", observe,
            eviction_threshold_bytes=8 * GIB,
        )
        small = capacity_plan(
            1, [], "/docker", "/tmp", observe,
            eviction_threshold_bytes=2 * GIB,
        )
        self.assertEqual(default[0]["required_bytes"], 10 * GIB)
        self.assertEqual(large[0]["required_bytes"], 18 * GIB)
        self.assertEqual(small[0]["required_bytes"], 10 * GIB)

    def test_binary_quantity_parser_rejects_ambiguous_capacity(self):
        self.assertEqual(parse_binary_quantity("8Gi"), 8 * GIB)
        self.assertEqual(parse_binary_quantity("128Mi"), 128 * MIB)
        for invalid in ("", "0Gi", "8G", "8%", "-1Gi", " 8Gi"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                parse_binary_quantity(invalid)

    def test_invalid_unknown_and_large_inputs(self):
        # Missing observations fail closed; arbitrary-size arithmetic cannot
        # wrap to a small successful budget. No node/image effects occur here.
        for nodes, sizes in ((0, []), (-1, []), (True, []), (1, [-1]), (1, [True])):
            with self.subTest(nodes=nodes, sizes=sizes), self.assertRaises(ValueError):
                capacity_plan(nodes, sizes, "/d", "/t")
        for threshold in (0, -1, True):
            with self.subTest(threshold=threshold), self.assertRaises(ValueError):
                capacity_plan(
                    1, [], "/d", "/t",
                    eviction_threshold_bytes=threshold,
                )
        with self.assertRaises(ValueError):
            capacity_plan(1, [], "/d", "/t", lambda _: (1, -1, 100))
        plan = capacity_plan(2 ** 64, [2 ** 64], "/d", "/t", lambda p: (p, 2 ** 64, 1_000_000))
        with self.assertRaises(ValueError):
            require_capacity(plan)

    def test_daemon_observation_and_alias_decision_table(self):
        # K4: local socket + absolute observed root + complete image metadata
        # admits calculation; remote daemon / unknown metadata fails before
        # any mutation. Two tags for one ID consume one image-size budget.
        records = [{"Id": "sha256:a", "Size": 10}] * 2
        with patch.dict(os.environ, {}, clear=True), \
                patch(__name__ + ".docker_output", side_effect=["unix:///fixture.sock", "/docker", json.dumps(records)]) as docker, \
                patch(__name__ + ".capacity_plan", return_value=[]) as plan:
            self.assertEqual(inspect_local_capacity(4, ["a", "alias"]), [])
            self.assertEqual(plan.call_args.args[:3], (4, [10], "/docker"))
            self.assertEqual(docker.call_count, 3)
        for endpoint in ("tcp://remote:2376", "ssh://remote", ""):
            with self.subTest(endpoint=endpoint), patch.dict(os.environ, {}, clear=True), \
                    patch(__name__ + ".docker_output", return_value=endpoint) as docker:
                with self.assertRaises(ValueError):
                    inspect_local_capacity(1, [])
                self.assertEqual(docker.call_count, 1)
        with patch.dict(os.environ, {"DOCKER_HOST": "tcp://remote:2376"}, clear=True), \
                patch(__name__ + ".docker_output") as docker:
            with self.assertRaises(ValueError):
                inspect_local_capacity(1, [])
            docker.assert_not_called()
        # Docker's explicit context wins over DOCKER_HOST; observation errors
        # propagate instead of becoming a zero-byte successful fixture.
        with patch.dict(os.environ, {"DOCKER_CONTEXT": "fixture", "DOCKER_HOST": "unix:///ignored"}, clear=True), \
                patch(__name__ + ".docker_output", side_effect=OSError("unobservable")):
            with self.assertRaises(OSError):
                inspect_local_capacity(1, [])


def main() -> None:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--nodes", type=int)
    parser.add_argument("--eviction-threshold")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("images", nargs="*")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(CapacityTests))
        raise SystemExit(0 if result.wasSuccessful() else 1)
    eviction_threshold_bytes = (
        parse_binary_quantity(args.eviction_threshold)
        if args.eviction_threshold is not None else None
    )
    plan = inspect_local_capacity(args.nodes, args.images, eviction_threshold_bytes)
    require_capacity(plan)
    print("k3d capacity admitted: " + json.dumps(plan, sort_keys=True))


if __name__ == "__main__":
    main()
