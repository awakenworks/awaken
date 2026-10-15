#!/usr/bin/env python3
"""Exercise the recovery drill's real admission before any Docker effect."""

import os
import json
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tempfile
import unittest
import _strict_unittest as strict_unittest
from _native_shell import bash_executable


DRILL = Path(__file__).resolve().with_name("postgres_recovery_drill.sh")
DEADLINE = DRILL.with_name("deadline.py")


def directory_alias(alias, target):
    if os.name == "nt":
        # Git Bash recognizes directory junctions as links (-L). Unlike
        # symbolic links they require no Windows symlink privilege, while
        # preserving both physical-parent admission and linked-leaf refusal.
        import _winapi
        _winapi.CreateJunction(str(target), str(alias))
    else:
        alias.symlink_to(target, target_is_directory=True)


def invoke_fixture(command, **kwargs):
    # Setup/diagnostics are not subsecond startup SLOs. One shared deadline
    # bounds the complete fixture and cleans its process group on timeout;
    # subprocess's longer watchdog diagnoses a failure in that same owner.
    return subprocess.run([sys.executable, str(DEADLINE), "60", "--", *command],
                          timeout=75, **kwargs)


def fixture_environment(binary):
    observer = binary / "fixture-resource-observer"
    values = {}
    if os.name == "nt":
        from _native_test_launcher import native_fixture_launcher
        # Native Python cannot execute a shebang fixture. Both commands enter
        # the same private dispatcher; never fall through to a real Docker.
        observer = observer.with_suffix(".exe")
        for executable in (binary / "docker.exe", binary / "python3.exe", observer):
            shutil.copyfile(native_fixture_launcher(), executable)
        values.update(AWAKEN_FIXTURE_PROGRAM=sys.executable,
                      AWAKEN_FIXTURE_SCRIPT=str(binary / "docker"))
    values["AWAKEN_HOST_ADMISSION"] = str(observer)
    values["AWAKEN_TEST_DOCKER_STATE"] = str(binary / "docker-state.json")
    return values


def recovery_root_argument(value):
    if os.name != "nt" or not str(value):
        return str(value)
    # The real producer exports a Bash path, not a native Windows path. Keep
    # traversal components lexical so the negative partition stays meaningful.
    return subprocess.check_output(
        [bash_executable(), "-c", 'cygpath -u -- "$1"', "fixture-path", str(value)],
        text=True, timeout=15,
    ).strip()


def write_startup_docker(path):
    """One fake Docker boundary for real producer startup and cleanup tests."""
    path.write_text(f"#!{sys.executable}\n" + '''import hashlib, json, os, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
if args and (args[0] == "-c" or args[0].endswith(".py")):
    # This producer test mocks its storage/container substrate. Keep capacity
    # eligible through the explicit fixture observer, rather than making an
    # empty fake Docker test depend on the host's real free disk. Real gates
    # still execute their unchanged disk threshold through the real Python.
    if Path(args[0]).name == "gate_tools.py" and args[1:2] == ["disk"]:
        args = [args[0], "resources", "--profile", "cargo"]
    # Windows CRT execv does not quote whitespace-bearing argv. Enter Python
    # through subprocess so -c code, spaces, and quotes remain single arguments.
    sys.exit(subprocess.run([sys.executable, *args]).returncode)
if "--profile" in args:
    eligible = os.environ.get("AWAKEN_TEST_REFUSE_RESOURCES") != "1"
    print(json.dumps({"schema_version": 1, "profile": "cargo", "eligible": eligible,
                      "problems": [] if eligible else ["fixture pressure"]}))
    sys.exit(0 if eligible else 75)
mode = os.environ.get("AWAKEN_TEST_STARTUP_FAILURE", "create")
if report := os.environ.get("AWAKEN_TEST_DOCKER_CALLS"):
    with open(report, "a") as out:
        out.write(json.dumps(args) + "\\n")
state_path = Path(os.environ.get("AWAKEN_TEST_DOCKER_STATE", str(Path(__file__).with_name("docker-state.json"))))
state = json.loads(state_path.read_text()) if state_path.exists() else {"containers": {}, "volumes": {}}
if args[0] == "info":
    sys.exit(0)
if args[0] == "ps":
    if mode == "cleanup-discover":
        sys.exit(23)
    owner = next((arg.removeprefix("label=io.awaken.ci.postgres-run=")
                  for arg in args if arg.startswith("label=io.awaken.ci.postgres-run=")), None)
    for identifier, item in state["containers"].items():
        if owner is None or item["owner"] == owner:
            print(identifier)
    sys.exit(0)
if args[0] == "rm":
    if mode == "cleanup-remove":
        sys.exit(31)
    result = 0
    for selected in (arg for arg in args[1:] if not arg.startswith("-")):
        identifier = next((key for key, item in state["containers"].items()
                           if selected in (key, item["name"])), None)
        if identifier is None:
            result = 1
            continue
        state["containers"].pop(identifier)
        if "--volumes" in args or "-v" in args:
            state["volumes"] = {key: item for key, item in state["volumes"].items()
                                if item["container"] != identifier or not item["anonymous"]}
    state_path.write_text(json.dumps(state))
    sys.exit(result)
if args[:2] == ["image", "inspect"]:
    # The gate's tool precheck asks this before any phase runs. Answer it
    # decisively: a probe that neither confirms nor denies the image is
    # undecidable, and an undecidable probe now refuses the gate outright --
    # correctly, but it would refuse before these startup phases could run.
    print("[]")
    sys.exit(0)
if args[0] == "run":
    name = args[args.index("--name") + 1]
    owner = next(arg.removeprefix("io.awaken.ci.postgres-run=")
                 for arg in args if arg.startswith("io.awaken.ci.postgres-run="))
    identifier = hashlib.sha256(name.encode()).hexdigest()
    state["containers"][identifier] = {"name": name, "owner": owner}
    mount = next((value for value in args if value.endswith(":/recovery-base")), None)
    if mount is not None:
        state["volumes"]["data-" + identifier] = {"container": identifier, "anonymous": True}
    state_path.write_text(json.dumps(state))
    if mount is None:
        sys.exit(73 if mode == "object-create" else 0)
    base = Path(mount.removesuffix(":/recovery-base"))
    if report := os.environ.get("AWAKEN_TEST_ALLOCATION"):
        Path(report).write_text(json.dumps({
            "root": str(base.parent), "base": base.is_dir(),
            "wal": (base.parent / "wal").is_dir(),
        }))
    sys.exit(73 if mode == "create" else 0)
if args[0] == "port":
    print("invalid" if mode == "port" else "127.0.0.1:49152")
    sys.exit(0)
if args[0] == "exec":
    sys.exit(0 if mode == "object-create" else 1)
if args[0] in ("inspect", "logs"):
    print("fixture " + args[0])
    sys.exit(79 if mode == "create" else 0)
sys.exit(73)
''')
    path.chmod(0o755)
    python = path.with_name("python3")
    python.write_text("#!/usr/bin/env bash\nexec "
                      + shlex.quote(sys.executable.replace("\\", "/")) + " "
                      + shlex.quote(str(path).replace("\\", "/")) + ' "$@"\n')
    python.chmod(0o755)
    observer = path.with_name("fixture-resource-observer")
    observer.write_text('#!/bin/sh\nprintf \'%s\\n\' \'{"schema_version":1,"profile":"cargo","eligible":true,"problems":[]}\'\n')
    observer.chmod(0o755)


class RecoveryRootTests(unittest.TestCase):
    def test_fixture_python_preserves_native_argument_boundaries(self):
        # The same PATH dispatcher used by the producer must preserve Python's
        # entire -c program and its whitespace/quote-bearing positional values.
        with tempfile.TemporaryDirectory(prefix="awaken-pg-python-argv-") as tmp:
            binary = Path(tmp).resolve()
            write_startup_docker(binary / "docker")
            environment = dict(os.environ, **fixture_environment(binary),
                               PATH=f"{binary}{os.pathsep}{os.environ['PATH']}")
            arguments = ["space value", 'embedded "quote"', "trailing\\"]
            result = invoke_fixture(
                [bash_executable(), "-c", 'python3 -c "$1" "${@:2}"', "fixture-python",
                 "import json, sys; print(json.dumps(sys.argv[1:]))", *arguments],
                env=environment, text=True, capture_output=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), arguments)

    def test_producer_allocates_and_cleans_only_its_own_directory(self):
        # P1 absolute/trailing-slash/symlink TMPDIR x failed Docker startup:
        # the real pg_tests producer allocates base+wal under that physical
        # parent and removes only this new root. An inherited caller-owned
        # AWAKEN_RECOVERY_ROOT must remain byte-for-byte untouched (P2).
        with tempfile.TemporaryDirectory(prefix="awaken-recovery-producer-test-") as tmp:
            fixture = Path(tmp).resolve()
            parent = fixture / "configured parent"
            parent.mkdir()
            alias = fixture / "parent-alias"
            directory_alias(alias, parent)
            caller = fixture / "caller-owned"
            caller.mkdir()
            (caller / "sentinel").write_bytes(b"preserved")
            binary = fixture / "bin"
            binary.mkdir()
            report = fixture / "allocation.json"
            docker = binary / "docker"
            write_startup_docker(docker)
            fixture_values = fixture_environment(binary)
            for selected in (parent, f"{parent}///", alias):
                with self.subTest(rule="P1/P2", parent=str(selected)):
                    report.unlink(missing_ok=True)
                    result = invoke_fixture(
                        [bash_executable(), str(DRILL.with_name("pg_tests.sh")), "--require-docker"],
                        env=dict(os.environ, TMPDIR=str(selected),
                                 **fixture_values,
                                 PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                                 CARGO_TARGET_DIR=str(fixture / "target"),
                                 AWAKEN_ADMISSION_PROFILE="cargo",
                                 AWAKEN_RECOVERY_ROOT=str(caller),
                                 AWAKEN_TEST_ALLOCATION=str(report)),
                        text=True, capture_output=True,
                    )
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn("Docker could not create", result.stderr)
                    allocated = json.loads(report.read_text())
                    self.assertTrue(allocated["base"] and allocated["wal"])
                    path = Path(allocated["root"])
                    self.assertEqual(path.parent, parent)
                    self.assertFalse(path.exists(), "P1 cleanup after failed startup")
                    self.assertEqual((caller / "sentinel").read_bytes(), b"preserved")
                    state = json.loads(Path(fixture_values["AWAKEN_TEST_DOCKER_STATE"]).read_text())
                    self.assertEqual(state, {"containers": {}, "volumes": {}}, "P1 anonymous data volume removed")

    def test_startup_failures_report_phases_and_diagnose_before_cleanup(self):
        # Cause/effect table: C1 create fails (possibly accepted remotely), C2
        # port response malformed, C3 final TCP readiness never arrives; C4
        # diagnostics also fail. Effects: E1 retain original exit 1; E2 trace the
        # exact failed phase/status even when output was suppressed; E3 inspect
        # and logs precede cleanup, only for the attempted container; E4 C4 never
        # masks C1. P13=C1+C4, P14=C2, P15=C3 => E1-E3 (and E4 for P13).
        # P16: C5 object creation fails after PG is ready => diagnose both exact
        # attempted containers before cleanup. C6 host image storage is shared
        # => E5 MinIO requests its own 4 GiB tmpfs instead of inheriting host free
        # capacity. Assert the real Docker invocation, not a parallel config.
        # This fixture is deliberately volatile; MinIO power-loss persistence
        # is outside this PG PITR and S3 copy/restore test's evidence boundary.
        # No cargo or real Docker is run; invoke the canonical producer itself.
        # P17 startup and diagnostics keep the producer's normal 30s operation
        # limit; only the intentionally never-ready poll uses a 1s budget.
        # Ambient host capacity is a separate cause, fixed eligible by this
        # fixture; it cannot impersonate one of the four startup failures.
        with tempfile.TemporaryDirectory(prefix="awaken-pg-diagnostics-") as tmp:
            fixture = Path(tmp).resolve()
            binary = fixture / "bin"
            binary.mkdir()
            write_startup_docker(binary / "docker")
            fixture_values = fixture_environment(binary)
            state_path = Path(fixture_values["AWAKEN_TEST_DOCKER_STATE"])
            foreign_id = "f" * 64
            foreign = {"containers": {foreign_id: {"name": "foreign-fixture", "owner": "2" * 32}},
                       "volumes": {"foreign-data": {"container": foreign_id, "anonymous": True}}}
            calls = fixture / "calls.jsonl"
            for mode, phase, status in (("create", "postgres-create", 73),
                                        ("port", "postgres-port", 0),
                                        ("ready", "postgres-ready", 1),
                                        ("object-create", "object-create", 73)):
                with self.subTest(mode=mode):
                    calls.unlink(missing_ok=True)
                    state_path.write_text(json.dumps(foreign))
                    result = invoke_fixture(
                        [bash_executable(), str(DRILL.with_name("pg_tests.sh")), "--require-docker"],
                        env=dict(os.environ, TMPDIR=str(fixture),
                                 **fixture_values,
                                 PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                                 CARGO_TARGET_DIR=str(fixture / "target"),
                                 AWAKEN_ADMISSION_PROFILE="cargo",
                                 AWAKEN_TEST_STARTUP_FAILURE=mode,
                                 AWAKEN_TEST_DOCKER_CALLS=str(calls),
                                 AWAKEN_PG_READY_TIMEOUT_SECONDS="1",
                                 AWAKEN_PG_OPERATION_TIMEOUT_SECONDS="30"),
                        text=True, capture_output=True,
                    )
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(f"stage={phase} event=start", result.stderr)
                    self.assertIn(f"stage={phase} event=finish status={status}", result.stderr)
                    commands = [json.loads(line) for line in calls.read_text().splitlines()]
                    verbs = [command[0] for command in commands]
                    self.assertLess(verbs.index("inspect"), verbs.index("rm"))
                    self.assertLess(verbs.index("logs"), verbs.index("rm"))
                    removals = [c for c in commands if c[0] == "rm"]
                    self.assertTrue(all("--volumes" in command for command in removals))
                    self.assertEqual(json.loads(state_path.read_text()), foreign,
                                     "owned containers/data removed; foreign container/data retained")
                    creations = [c for c in commands if c[0] == "run"]
                    created = {c[c.index("--name") + 1] for c in creations}
                    for verb in ("inspect", "logs"):
                        diagnosed = {c[-1] for c in commands if c[0] == verb}
                        self.assertEqual(diagnosed, created, "P13-P16 exact attempted containers")
                    if mode == "object-create":
                        self.assertEqual(len(creations), 2, "P16 PG then MinIO")
                        self.assertEqual(creations[1][creations[1].index("--tmpfs") + 1],
                                         "/data:rw,nosuid,nodev,noexec,size=4g", "P16/E5")
                    self.assertNotIn("Config.Env", result.stderr)
                    self.assertFalse(list(fixture.glob("awaken-pg-recovery.*")))

            # P18 a valid host observation explicitly refuses resources -> exact
            # 75, no container creation/allocation. Unlike P13-P16 this fails
            # before a substrate exists, so diagnostics/cleanup cannot mask it.
            observer = binary / "fixture-resource-observer"
            observer.write_text('#!/bin/sh\nprintf \'%s\\n\' \'{"schema_version":1,"profile":"cargo","eligible":false,"problems":["fixture pressure"]}\'\nexit 75\n')
            calls.unlink(missing_ok=True)
            result = invoke_fixture(
                [bash_executable(), str(DRILL.with_name("pg_tests.sh")), "--require-docker"],
                env=dict(os.environ, TMPDIR=str(fixture),
                         **fixture_values,
                         PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                         CARGO_TARGET_DIR=str(fixture / "target"),
                         AWAKEN_TEST_REFUSE_RESOURCES="1", AWAKEN_ADMISSION_PROFILE="cargo",
                         AWAKEN_TEST_DOCKER_CALLS=str(calls)),
                text=True, capture_output=True,
            )
            self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
            commands = [json.loads(line) for line in calls.read_text().splitlines()]
            self.assertFalse(any(command[0] in ("run", "rm", "exec") for command in commands))
            self.assertFalse(list(fixture.glob("awaken-pg-recovery.*")))

    def test_cleanup_failure_retains_resources_and_original_failure(self):
        # C7 discovery fails or C8 removal fails after a real producer readiness
        # failure. E6 preserve the original1, expose23/31 in its cleanup phase,
        # and retain the uncertain container/anonymous volume/recovery root.
        # Every Docker effect is simulated; parent disk capacity is fixed by the
        # same fixture observer, never borrowed from the executing workstation.
        for mode, phase, status in (("cleanup-discover", "cleanup-discover", 23),
                                    ("cleanup-remove", "cleanup-remove", 31)):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(prefix="awaken-pg-cleanup-failure-") as tmp:
                fixture = Path(tmp).resolve()
                binary = fixture / "bin"
                binary.mkdir()
                write_startup_docker(binary / "docker")
                values = fixture_environment(binary)
                allocation = fixture / "allocation.json"
                result = invoke_fixture(
                    [bash_executable(), str(DRILL.with_name("pg_tests.sh")), "--require-docker"],
                    env=dict(os.environ, TMPDIR=str(fixture), **values,
                             PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                             CARGO_TARGET_DIR=str(fixture / "target"), AWAKEN_ADMISSION_PROFILE="cargo",
                             AWAKEN_TEST_STARTUP_FAILURE=mode, AWAKEN_TEST_ALLOCATION=str(allocation),
                             AWAKEN_PG_READY_TIMEOUT_SECONDS="1", AWAKEN_PG_OPERATION_TIMEOUT_SECONDS="30"),
                    text=True, capture_output=True,
                )
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("stage=postgres-ready event=finish status=1", result.stderr)
                self.assertIn(f"stage={phase} event=finish status={status}", result.stderr)
                self.assertIn("retaining its owned recovery directory", result.stderr)
                state = json.loads(Path(values["AWAKEN_TEST_DOCKER_STATE"]).read_text())
                self.assertEqual(len(state["containers"]), 1)
                self.assertEqual(len(state["volumes"]), 1)
                self.assertTrue(Path(json.loads(allocation.read_text())["root"]).is_dir())

    def test_configured_parent_and_exact_owned_leaf_control_admission(self):
        # Causes: C1 the selected TMPDIR (absolute, trailing slash, relative or
        # symlink alias); C2 an exact six-ASCII-alphanumeric allocation leaf;
        # C3 a direct root with direct base/wal directories. Effects: E1 admit
        # only C1+C2+C3 and propagate the first Docker failure; E2 otherwise
        # reject before writing content or invoking Docker. The fixture owns
        # every directory; no actual Docker operation runs in this unit test.
        # Decision table: R1 all C1 representations x C2+C3 => E1; R2 each
        # malformed/outside/descendant leaf => E2; R3 absent or symlinked root,
        # base or wal => E2. A six-character traversal suffix is deliberately
        # included: shell case '?' matches '/' and is not a safe leaf grammar.
        with tempfile.TemporaryDirectory(prefix="awaken-recovery-root-test-") as tmp:
            fixture = Path(tmp).resolve()
            parent = fixture / "configured parent"
            parent.mkdir()
            alias = fixture / "parent-alias"
            directory_alias(alias, parent)
            binary = fixture / "bin"
            binary.mkdir()
            docker = binary / "docker"
            docker.write_text(
                '#!/bin/sh\nprintf "called\\n" >> "$AWAKEN_TEST_DOCKER_CALLS"\nexit 73\n'
            )
            docker.chmod(0o755)
            calls = fixture / "docker-calls"
            owned = parent / "awaken-pg-recovery.A1b2C3"
            owned.mkdir()
            for leaf in ("base", "wal"):
                (owned / leaf).mkdir()

            def invoke(root, selected_parent):
                calls.unlink(missing_ok=True)
                env = dict(
                    os.environ,
                    PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                    TMPDIR=str(selected_parent),
                    AWAKEN_RECOVERY_ROOT=recovery_root_argument(root),
                    AWAKEN_TEST_DOCKER_CALLS=str(calls),
                )
                return invoke_fixture(
                    [bash_executable(), str(DRILL), "fixture-pg", "fixture-objects", "fixture-image"],
                    cwd=fixture, env=env, text=True, capture_output=True,
                )

            for selected_parent in (parent, f"{parent}/", "configured parent", alias):
                with self.subTest(rule="R1", parent=str(selected_parent)):
                    result = invoke(owned, selected_parent)
                    self.assertEqual(result.returncode, 73, result.stderr)
                    self.assertTrue(calls.exists(), "R1 admitted actual Docker boundary")
                    (owned / "content-before-target.txt").unlink()

            outside = fixture / "awaken-pg-recovery.A1b2C3"
            malformed = [
                parent / "awaken-pg-recovery.short",
                parent / "awaken-pg-recovery.toolong",
                parent / "awaken-pg-recovery.A1b2C_",
                owned / "child",
                outside,
                parent,
            ]
            for root in malformed:
                root.mkdir(exist_ok=True)
                for leaf in ("base", "wal"):
                    (root / leaf).mkdir(exist_ok=True)
            # Keep the lexical traversal in the input; every target is still
            # inside our private fixture, even if a broken guard accepts it.
            (parent / "awaken-pg-recovery.A").mkdir()
            traversal = f"{parent}/awaken-pg-recovery.A/../."
            for root in [*malformed, traversal, "", parent / "awaken-pg-recovery.Absent"]:
                with self.subTest(rule="R2", root=str(root)):
                    result = invoke(root, parent)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertFalse(calls.exists(), "R2 no Docker effects")

            for leaf in (None, "base", "wal"):
                path = owned if leaf is None else owned / leaf
                retained = path.with_name(path.name + "-retained")
                path.rename(retained)
                for symlink in (False, True):
                    if symlink:
                        directory_alias(path, retained)
                    try:
                        with self.subTest(rule="R3", leaf=leaf, symlink=symlink):
                            result = invoke(owned, parent)
                            self.assertEqual(result.returncode, 2, result.stderr)
                            self.assertFalse(calls.exists(), "R3 no Docker effects")
                            self.assertFalse((retained / "content-before-target.txt").exists())
                    finally:
                        # A killed mutant may have crossed the boundary. Keep
                        # that assertion as the failure, never a later fixture
                        # rename error, and clean only our private fixture.
                        if path.is_symlink():
                            path.unlink()
                        elif os.name == "nt" and path.is_junction():
                            path.rmdir()
                        (retained / "content-before-target.txt").unlink(missing_ok=True)
                        (owned / "content-before-target.txt").unlink(missing_ok=True)
                retained.rename(path)


if __name__ == "__main__":
    strict_unittest.main()
