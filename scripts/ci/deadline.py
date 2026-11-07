#!/usr/bin/env python3
"""Portable command deadline with whole-process-group cleanup.

A deadline measures execution, not time spent queued for host build
admission. The host tool (cargo_admission.py, fair-slots-v2) exports each grant
(AWAKEN_ADMISSION_GRANTED_AT / AWAKEN_ADMISSION_QUEUED_SECONDS) into the
admitted child. This file is the one place that turns grants into budgets: the
queue ledger. Every deadline owns a private ledger directory and lists it in
its descendants' AWAKEN_DEADLINE_LEDGERS. An admitted call made through
`--admit` (the `run_with_deadline` Cargo path) records its request in every
listed ledger. The call's post-admission entry then records the grant. Each
ancestor pauses its clock for that interval. A shell-held budget (pg_tests.sh)
reads its own ledger with `--queued-seconds`. A host tool that exports no grant
(the older fair-single-lane-v1 tool) gets no records, so every clock runs
exactly as before.
"""

from __future__ import annotations

import json
import os
import math
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path


TIMEOUT_EXIT = 124
RESOURCE_EXIT = 75
KILL_GRACE_SECONDS = 5

LEDGERS_VARIABLE = "AWAKEN_DEADLINE_LEDGERS"
REQUEST_VARIABLE = "AWAKEN_DEADLINE_REQUEST"
GRANTED_AT_VARIABLE = "AWAKEN_ADMISSION_GRANTED_AT"
QUEUED_VARIABLE = "AWAKEN_ADMISSION_QUEUED_SECONDS"
WAIT_VARIABLE = "AWAKEN_ADMISSION_WAIT_SECONDS"
LEDGER_PREFIX = "awaken-deadline-ledger."
# The host's default bound on waiting out transient pressure, used when its
# capabilities report none.
DEFAULT_HOST_WAIT_SECONDS = 1800
# A queued call may wait max(its budget, the host's wait bound) + this margin.
# That is never sooner than a budget that counted the queue would have stopped it.
QUEUE_MARGIN_SECONDS = 600
# After its grant, the --admit wrapper outlives the inner deadline by this much;
# the inner deadline, not the wrapper, reports the execution timeout.
ADMITTED_GRACE_SECONDS = 30
LEDGER_POLL_SECONDS = 2.0


class Cancelled(BaseException):
    def __init__(self, number: int):
        self.number = number


class QueueBoundExceeded(Exception):
    def __init__(self, record: dict):
        self.record = record


def _write_json(path: Path, value: dict) -> None:
    pending = path.with_name(path.name + ".pending")
    pending.write_text(json.dumps(value), encoding="utf-8")
    pending.replace(path)


def _read_json(path: Path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def ledger_directories(environment) -> list[Path]:
    return [Path(item) for item in environment.get(LEDGERS_VARIABLE, "").split(os.pathsep) if item]


def open_ledger() -> Path | None:
    """A private queue ledger, or None (nothing excluded) if TMPDIR refuses one."""
    try:
        return Path(tempfile.mkdtemp(prefix=LEDGER_PREFIX))
    except OSError:
        return None


def close_ledger(ledger: Path | None) -> None:
    if ledger is not None and ledger.name.startswith(LEDGER_PREFIX):
        shutil.rmtree(ledger, ignore_errors=True)


def queue_state(ledger: Path | None, now: float | None = None) -> tuple[float, list[dict]]:
    """Seconds descendants spent queued (a union of intervals) and the calls still queued."""
    if ledger is None:
        return 0.0, []
    now = time.time() if now is None else now
    intervals, queued = [], []
    try:
        paths = [path for path in ledger.iterdir() if path.suffix == ".json"]
    except OSError:
        paths = []
    for path in paths:
        record = _read_json(path)
        if not isinstance(record, dict) or not isinstance(record.get("requested_at_unix"), (int, float)):
            continue
        start = record["requested_at_unix"]
        end = record.get("granted_at_unix", record.get("ended_at_unix"))
        if end is None:
            queued.append({**record, "queued_seconds": now - start})
            end = now
        intervals.append((start, min(end, now)))
    excluded, reach = 0.0, -math.inf
    for start, end in sorted(intervals):
        start = max(start, reach)
        if end > start:
            excluded += end - start
            reach = end
    return excluded, queued


def host_grant_policy(environment) -> dict | None:
    """The queue-bound inputs when the host exports admission grants, else None.

    The host installs `awaken-build-admission` beside `cargo`. An older tool (no
    `grant_environment`), a missing observer or a plain Cargo returns None.
    """
    cargo = shutil.which("cargo", path=environment.get("PATH"))
    if cargo is None:
        return None
    observer = Path(cargo).parent / "awaken-build-admission"
    if not (observer.is_file() and os.access(observer, os.X_OK)):
        return None
    try:
        value = json.loads(subprocess.run([str(observer), "--capabilities"], env=environment,
                                          capture_output=True, text=True, timeout=30).stdout)
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    if (not isinstance(value, dict) or value.get("kind") != "host-admission-capabilities"
            or not isinstance(value.get("grant_environment"), dict)
            or value["grant_environment"].get("granted_at_unix") != GRANTED_AT_VARIABLE):
        return None
    wait = (value.get("transient_pressure_wait") or {}).get("default_seconds", DEFAULT_HOST_WAIT_SECONDS)
    raw = environment.get(WAIT_VARIABLE, "").strip()
    if raw.isascii() and raw.isdigit():
        wait = int(raw)
    return {"wait_seconds": wait if isinstance(wait, int) and wait >= 0 else DEFAULT_HOST_WAIT_SECONDS}


def cooperative_admission_problem(value) -> str | None:
    """None when the host can run the cooperative Formal root.

    Accepted: fair-single-lane-v1 with one execution slot (unchanged), or
    fair-slots-v2 that still runs formal work alone. Anything else is refused.
    """
    if (not isinstance(value, dict) or value.get("kind") != "host-admission-capabilities"
            or value.get("schema_version") != 1 or value.get("cooperative_formal") is not True):
        return "unsupported cooperative admission"
    if value.get("scheduler") == "fair-single-lane-v1" and value.get("execution_slots") == 1:
        return None
    if (value.get("scheduler") == "fair-slots-v2" and isinstance(value.get("exclusive_kinds"), list)
            and "formal" in value["exclusive_kinds"]):
        return None
    return "unsupported cooperative admission"


def require_cooperative_admission(observer: str) -> int:
    try:
        value = json.loads(subprocess.check_output([observer, "--capabilities"], text=True, timeout=60))
        problem = cooperative_admission_problem(value)
        if problem:
            raise ValueError(problem)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print("cooperative formal admission unavailable; install the committed host policy: " + str(error),
              file=sys.stderr)
        return RESOURCE_EXIT
    return 0


class AdmissionRequest:
    """One admitted call's queue interval, written into every listed ledger."""

    def __init__(self, ledgers: list[Path], bound: float):
        self.identifier = uuid.uuid4().hex
        self.paths = []
        record = {"requested_at_unix": time.time(), "queue_bound_seconds": bound}
        for ledger in ledgers:
            path = ledger / (self.identifier + ".json")
            try:
                _write_json(path, record)
            except OSError:
                continue  # A finished ancestor removed its ledger; it has no clock left.
            self.paths.append(path)

    def finish(self) -> None:
        """A call that never got its grant (refused, or stopped while queued) stops queueing now."""
        now = time.time()
        for path in self.paths:
            record = _read_json(path)
            if isinstance(record, dict) and record.get("granted_at_unix") is None:
                try:
                    _write_json(path, {**record, "ended_at_unix": now})
                except OSError:
                    pass


def record_grant(environment) -> None:
    """The post-admission entry marks its request granted.

    AWAKEN_ADMISSION_GRANTED_AT wins; else AWAKEN_ADMISSION_QUEUED_SECONDS after
    the request; else this entry's own start, which follows admission anyway.
    """
    identifier = environment.get(REQUEST_VARIABLE, "")
    if not (identifier.isascii() and identifier.isalnum()):
        return
    now = time.time()
    for ledger in ledger_directories(environment):
        path = ledger / (identifier + ".json")
        record = _read_json(path)
        if not isinstance(record, dict) or not isinstance(record.get("requested_at_unix"), (int, float)):
            continue
        requested, granted = record["requested_at_unix"], now
        try:
            exported = float(environment[GRANTED_AT_VARIABLE])
            if math.isfinite(exported) and requested - 1 <= exported <= now + 1:
                granted = exported
        except (KeyError, ValueError):
            try:
                queued = float(environment[QUEUED_VARIABLE])
                if math.isfinite(queued) and queued >= 0:
                    granted = min(now, requested + queued)
            except (KeyError, ValueError):
                pass
        try:
            _write_json(path, {**record, "granted_at_unix": min(now, max(requested, granted))})
        except OSError:
            pass


def terminate_group(process: subprocess.Popen[bytes]) -> None:
    """Terminate the command and every descendant in its new process group."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    # The leader exiting is not evidence that its descendants have exited.
    # Reap it while retaining the same group through the complete TERM grace.
    until = time.monotonic() + KILL_GRACE_SECONDS
    while True:
        process.poll()
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            process.wait()
            return
        remaining = until - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(remaining, 0.01))
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def run(seconds: float | None, command: list[str], *, stdout=None, stderr=None, environment=None,
        admission_bound: float | None = None, report: dict | None = None) -> int:
    """Run `command` for at most `seconds` of execution (None: no execution bound).

    Time a descendant spends queued for host admission is not execution. With
    `admission_bound`, this call is itself one admitted request, and a queue
    longer than the bound stops it with 75. `report` receives `queued_seconds`.
    """
    if (seconds is not None and (not math.isfinite(seconds) or seconds <= 0)) or not command:
        return 2
    stdio = {name: value for name, value in (('stdout', stdout), ('stderr', stderr)) if value is not None}
    if os.name == "nt":
        from _windows_job import WindowsJob

        if environment is not None:
            stdio['env'] = environment
        with WindowsJob() as job:
            process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                                        "--windows-job-child", job.name, "--", *command],
                                       **stdio)
            try:
                return process.wait(timeout=seconds)
            except subprocess.TimeoutExpired:
                job.close()
                # A trampoline still starting Python may not have joined yet.
                # It cannot launch the command before joining the owned Job.
                if process.poll() is None:
                    process.kill()
                process.wait()
                return TIMEOUT_EXIT
            except BaseException:
                job.close()
                if process.poll() is None:
                    process.kill()
                process.wait()
                raise
    child_environment = dict(os.environ if environment is None else environment)
    # A request belongs to the one post-admission entry that receives it.
    child_environment.pop(REQUEST_VARIABLE, None)
    ledger = open_ledger()
    if ledger is not None:
        child_environment[LEDGERS_VARIABLE] = os.pathsep.join(
            [str(ledger), *map(str, ledger_directories(child_environment))])
    request = None
    process = None
    pending_signal = None
    cleaning = False
    previous_handlers = {}

    def interrupted(number: int, _frame: object) -> None:
        nonlocal pending_signal
        if cleaning:
            return
        pending_signal = pending_signal or number
        # Never raise from a signal handler: Popen may hold its non-reentrant
        # waitpid lock before its cleanup guard has been established. Retain
        # cancellation during construction, waiting and repeated signals; the
        # ordinary wait boundary below owns unwinding and group cleanup.

    def cleanup() -> None:
        nonlocal cleaning
        cleaning = True
        if process is not None:
            terminate_group(process)

    started = time.monotonic()
    try:
        for number in (signal.SIGTERM, signal.SIGINT):
            previous_handlers[number] = signal.signal(number, interrupted)
        if admission_bound is not None:
            request = AdmissionRequest(ledger_directories(child_environment), admission_bound)
            child_environment[REQUEST_VARIABLE] = request.identifier
        process = subprocess.Popen(command, start_new_session=True, env=child_environment, **stdio)
        started = time.monotonic()
        until = math.inf if seconds is None else started + seconds
        while True:
            if pending_signal is not None:
                raise Cancelled(pending_signal)
            now = time.monotonic()
            if now >= until:
                # Read the ledger only when the plain clock says time is up.
                excluded, queued = queue_state(ledger)
                for record in queued:
                    if record["queued_seconds"] > record.get("queue_bound_seconds", math.inf):
                        raise QueueBoundExceeded(record)
                remaining = seconds - (now - started - excluded)
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command, seconds)
                # While a descendant queues the clock is paused: look again soon.
                until = now + (min(remaining, LEDGER_POLL_SECONDS) if queued else remaining)
            try:
                result = process.wait(timeout=min(until - now, 0.05))
            except subprocess.TimeoutExpired:
                continue
            if pending_signal is not None:
                raise Cancelled(pending_signal)
            break
        if result < 0:
            # A child killed by a signal reports a negative Python return code.
            # Shell callers need the conventional 128 + signal status; otherwise
            # SystemExit(-15) wraps to 241 and hides the reason for failure.
            number = -result
            print(f"command terminated by signal {number}", file=sys.stderr)
            return 128 + number
        return result
    except subprocess.TimeoutExpired:
        cleanup()
        excluded, _ = queue_state(ledger)
        if excluded >= 1:
            print(f"deadline: {seconds:g}s of execution spent ({time.monotonic() - started:.0f}s wall; "
                  f"{excluded:.0f}s queued for host build admission was not counted)", file=sys.stderr)
        return TIMEOUT_EXIT
    except QueueBoundExceeded as exceeded:
        cleanup()
        print(f"deadline: queued {exceeded.record['queued_seconds']:.0f}s for a host build slot, past the "
              f"{exceeded.record.get('queue_bound_seconds', 0):.0f}s queue bound (max(budget, {WAIT_VARIABLE}) + "
              f"{QUEUE_MARGIN_SECONDS}s); the command did not run. The host is busy: retry later; "
              f"`awaken-build-admission --status` shows who holds the slots.", file=sys.stderr)
        return RESOURCE_EXIT
    except Cancelled as cancellation:
        cleanup()
        print(f"deadline cancelled by signal {cancellation.number}", file=sys.stderr)
        return 128 + cancellation.number
    except BaseException:
        cleanup()
        raise
    finally:
        for number, handler in previous_handlers.items():
            signal.signal(number, handler)
        if request is not None:
            request.finish()
        if report is not None:
            report["queued_seconds"] = round(queue_state(ledger)[0], 3)
        close_ledger(ledger)


def admitted(raw_seconds: str, command: list[str]) -> int:
    """`run_with_deadline --admit`: one host admission, then `raw_seconds` of execution.

    Cargo's external-command protocol runs this file again as
    `cargo-awaken-ci-deadline` after admission; that inner deadline owns the
    execution budget. Here only the queue is bounded, and only when the host
    exports grants: with the older tool the call is unbounded while queued, as before.
    """
    try:
        seconds = float(raw_seconds)
    except ValueError:
        return 2
    if not math.isfinite(seconds) or seconds <= 0 or not command:
        return 2
    environment = dict(os.environ)
    try:
        shim = Path(tempfile.mkdtemp(prefix="awaken-cargo-deadline."))
    except OSError as error:
        print(f"cannot create the Cargo deadline entry: {error}", file=sys.stderr)
        return 2
    try:
        (shim / "cargo-awaken-ci-deadline").symlink_to(Path(__file__).resolve())
        environment["PATH"] = str(shim) + os.pathsep + environment.get("PATH", "")
        argv = ["cargo", "awaken-ci-deadline", raw_seconds, "--", *command]
        # An already admitted job's Cargo inherits its slot and never queues.
        policy = None if environment.get("AWAKEN_CARGO_ADMITTED") == "1" else host_grant_policy(environment)
        if policy is None:
            return run(None, argv, environment=environment)
        bound = max(seconds, policy["wait_seconds"]) + QUEUE_MARGIN_SECONDS
        return run(seconds + ADMITTED_GRACE_SECONDS, argv, environment=environment, admission_bound=bound)
    finally:
        shutil.rmtree(shim, ignore_errors=True)


def ledger_command(argv: list[str]) -> int | None:
    """The shell-budget and capability entries; None when argv is an ordinary deadline."""
    option = argv[1] if len(argv) > 1 else ""
    if option == "--admit":
        if len(argv) < 5 or argv[3] != "--":
            return 2
        return admitted(argv[2], argv[4:])
    if option == "--require-cooperative" and len(argv) == 3:
        return require_cooperative_admission(argv[2])
    if option == "--open-ledger" and len(argv) == 2:
        ledger = open_ledger()
        if ledger is None:
            return 1
        print(ledger)
        return 0
    if option in ("--queued-seconds", "--close-ledger") and len(argv) == 3:
        ledger = Path(argv[2])
        if not ledger.name.startswith(LEDGER_PREFIX):
            return 2
        if option == "--close-ledger":
            close_ledger(ledger)
        else:
            print(int(queue_state(ledger)[0]))
        return 0
    return None


def main(argv: list[str]) -> int:
    if argv[1:2] == ["--windows-job-child"]:
        if os.name != "nt" or len(argv) < 5 or argv[3] != "--":
            return 2
        from _windows_job import attach_current_process

        attach_current_process(argv[2])
        command = argv[4:]
        if command[0].casefold() in {"bash", "bash.exe"}:
            # CreateProcess searches the current/system directories before PATH.
            # Resolve native Bash only after joining the owned Job; explicit
            # command paths retain the caller's authority.
            from _native_shell import bash_executable

            command = [bash_executable(), *command[1:]]
        return subprocess.call(command)
    # Cargo's external-command protocol inserts the subcommand name. The entry
    # is a temporary symlink to this owner, so admission completes before run()
    # starts its clock. Direct invocation retains exactly the same parser.
    entry = Path(argv[0]).name
    if entry.startswith("cargo-"):
        if argv[1:2] != [entry.removeprefix("cargo-")]:
            return 2
        argv = [argv[0], *argv[2:]]
        record_grant(os.environ)
    else:
        status = ledger_command(argv)
        if status is not None:
            return status
    if len(argv) < 3:
        print(f"usage: {Path(argv[0]).name} SECONDS -- COMMAND [ARG ...]", file=sys.stderr)
        return 2
    try:
        seconds = float(argv[1])
    except ValueError:
        print("deadline must be a positive number", file=sys.stderr)
        return 2
    command = argv[3:] if argv[2] == "--" else argv[2:]
    return run(seconds, command)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
