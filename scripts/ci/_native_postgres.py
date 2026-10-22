"""Owned native PostgreSQL fixture; SQL identity is required before admission."""
from __future__ import annotations

import json
import os
from pathlib import Path
import secrets
import subprocess
import time

from native_services import free_port, windows_executable


def validate_identity(value: dict, owner: str, port: int, data: Path) -> dict:
    if (not isinstance(value, dict) or value.get("cluster_name") != owner
            or value.get("port") != port
            or not isinstance(value.get("data_directory"), str)
            or Path(value["data_directory"]).resolve() != data.resolve()
            or not isinstance(value.get("started"), str) or not value["started"]
            or not isinstance(value.get("version"), str)
            or not any(word in value["version"].lower() for word in ("windows", "visual c++"))
            or not isinstance(value.get("version_num"), int)
            or not 160000 <= value["version_num"] < 170000):
        raise ValueError("PostgreSQL endpoint is not this owned native Windows 16 cluster")
    return value


class PostgresService:
    def __init__(self, binaries: Path, root: Path, output: Path, owner: str):
        if os.name != "nt":
            raise RuntimeError("native service validation requires Windows")
        self.binaries = {name: windows_executable(binaries / (name + ".exe"))
                         for name in ("postgres", "initdb", "psql", "pg_ctl")}
        self.root, self.output, self.owner = root, output, owner
        self.data = root / "data"
        self.port = free_port()
        self.process = None
        self.log = None
        self.identity = None
        self.password = secrets.token_hex(24)
        self.url = f"postgres://postgres:{self.password}@127.0.0.1:{self.port}/awaken_ci"
        self.environment = {key: value for key, value in os.environ.items()
                            if not key.upper().startswith("PG")}
        self.environment.update(PGHOST="127.0.0.1", PGPORT=str(self.port), PGUSER="postgres",
                                PGDATABASE="postgres", PGPASSWORD=self.password, PGCONNECT_TIMEOUT="3")

    def command(self, name: str, arguments: list[str], *, timeout: int = 30) -> str:
        result = subprocess.run([self.binaries[name]["path"], *arguments],
                                env=self.environment, capture_output=True, stdin=subprocess.DEVNULL,
                                timeout=timeout, creationflags=subprocess.CREATE_NO_WINDOW)
        if result.returncode:
            # Configuration can include credentials. Never copy argv/environment
            # or server connection errors into a public execution receipt.
            raise RuntimeError(f"owned PostgreSQL {name} failed with status {result.returncode}")
        return result.stdout.decode("utf-8").strip()

    def sql(self, statement: str) -> str:
        return self.command("psql", ["-X", "-qAt", "-w", "-v", "ON_ERROR_STOP=1",
                                     "-c", statement], timeout=8)

    def read_identity(self) -> dict:
        value = json.loads(self.sql("SELECT json_build_object("
            "'cluster_name', current_setting('cluster_name'),"
            "'port', current_setting('port')::int,"
            "'data_directory', current_setting('data_directory'),"
            "'started', pg_postmaster_start_time()::text,"
            "'version', version(), 'version_num', current_setting('server_version_num')::int)"))
        return validate_identity(value, self.owner, self.port, self.data)

    def assert_owned_pid(self) -> None:
        if self.process is None or self.process.poll() is not None:
            raise RuntimeError("owned PostgreSQL process is no longer running")
        lines = (self.data / "postmaster.pid").read_text(encoding="utf-8").splitlines()
        if (len(lines) < 2 or lines[0] != str(self.process.pid)
                or Path(lines[1]).resolve() != self.data.resolve()):
            raise RuntimeError("PostgreSQL PID/data ownership changed; refusing cluster control")

    def start(self) -> dict:
        self.root.mkdir()
        password_file = self.root / "init-password"
        password_file.write_text(self.password + "\n", encoding="utf-8")
        try:
            self.command("initdb", ["-D", str(self.data), "--username=postgres",
                                   "--auth=scram-sha-256", "--encoding=UTF8", "--locale=C",
                                   "--pwfile=" + str(password_file)], timeout=90)
        finally:
            password_file.unlink(missing_ok=True)
        self.log = (self.output / "postgres-server.log").open("ab")
        self.process = subprocess.Popen([self.binaries["postgres"]["path"], "-D", str(self.data),
            "-h", "127.0.0.1", "-p", str(self.port), "-c", "cluster_name=" + self.owner,
            "-c", "shared_buffers=32MB", "-c", "max_wal_size=128MB"],
            env=self.environment, stdout=self.log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NO_WINDOW)
        until = time.monotonic() + 45
        while time.monotonic() < until:
            if self.process.poll() is not None:
                raise RuntimeError("owned PostgreSQL process exited before readiness")
            try:
                self.identity = self.read_identity()
                self.assert_owned_pid()
                self.sql("CREATE DATABASE awaken_ci")
                return self.receipt()
            except (RuntimeError, subprocess.TimeoutExpired):
                time.sleep(0.1)
        raise TimeoutError("owned PostgreSQL cluster did not become ready")

    def receipt(self) -> dict:
        return {"kind": "native-windows-process", "binaries": self.binaries,
                "pid": self.process.pid, "owner": self.owner, "identity": self.identity,
                "loopback_port": self.port, "database": "awaken_ci",
                "authentication": "fresh per-run SCRAM credential; not recorded"}

    def verify(self) -> None:
        self.assert_owned_pid()
        if self.read_identity() != self.identity:
            raise RuntimeError("owned PostgreSQL identity changed during verification")

    def close(self) -> dict:
        try:
            if self.process is None:
                return {"stopped": True, "not_started": True}
            if self.process.poll() is None:
                self.assert_owned_pid()
                self.command("pg_ctl", ["stop", "-D", str(self.data), "-m", "fast", "-w", "-t", "10"], timeout=15)
            self.process.wait(timeout=10)
            return {"stopped": True, "pid": self.process.pid,
                    "exit_code": self.process.returncode, "method": "owned-pid-and-data-directory-fast-stop"}
        finally:
            if self.log is not None:
                self.log.close()
