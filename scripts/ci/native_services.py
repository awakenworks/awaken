"""Native Windows service fixtures; no daemon, installer or container fallback."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import time


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def windows_executable(path: Path) -> dict:
    path = path.resolve(strict=True)
    with path.open("rb") as stream:
        if stream.read(2) != b"MZ":
            raise ValueError("native service must be a Windows PE executable")
        stream.seek(60)
        raw_offset=stream.read(4)
        if len(raw_offset)!=4:
            raise ValueError("native service has a truncated PE header")
        offset = struct.unpack("<I", raw_offset)[0]
        stream.seek(offset)
        signature=stream.read(4)
        machine=stream.read(2)
        if signature != b"PE\0\0" or len(machine)!=2 or struct.unpack("<H",machine)[0] != 0x8664:
            raise ValueError("native service must use the Windows AMD64 ABI")
    return {"path":str(path),"sha256":sha256(path),"abi":"windows-amd64"}


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1",0))
        return probe.getsockname()[1]


def decode_nats_info(line: bytes, owner: str, port: int) -> dict:
    if len(line)>65536 or not line.endswith(b"\n") or not line.startswith(b"INFO "):
        raise ValueError("NATS readiness requires a complete bounded INFO frame")
    info=json.loads(line[5:])
    if (not isinstance(info,dict) or info.get("server_name")!=owner
            or info.get("port")!=port or not isinstance(info.get("server_id"),str)
            or not info["server_id"]):
        raise ValueError("NATS endpoint does not belong to this fixture")
    return {key:info[key] for key in ("server_name","server_id","port","version")}


def nats_info(port: int, owner: str) -> dict:
    with socket.create_connection(("127.0.0.1",port),timeout=1) as connection:
        connection.settimeout(1)
        with connection.makefile("rb") as reader:
            return decode_nats_info(reader.readline(65537),owner,port)


class NatsService:
    """The retained process handle is the sole authority to stop this broker."""
    def __init__(self, executable: Path, root: Path, output: Path, owner: str):
        if os.name!="nt":
            raise RuntimeError("native service validation requires Windows")
        self.binary=windows_executable(executable)
        self.root=root
        self.output=output
        self.owner=owner
        self.port=free_port()
        self.process=None
        self.log=None
        self.identity=None

    def start(self) -> dict:
        self.root.mkdir()
        config=self.root/"nats.conf"
        config.write_text(f'listen: "127.0.0.1:{self.port}"\nserver_name: "{self.owner}"\n',encoding="utf-8")
        self.log=(self.output/"nats-server.log").open("ab")
        self.process=subprocess.Popen([self.binary["path"],"--config",str(config)],
            stdout=self.log,stderr=subprocess.STDOUT,stdin=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NO_WINDOW)
        until=time.monotonic()+30
        while time.monotonic()<until:
            if self.process.poll() is not None:
                raise RuntimeError("owned NATS process exited before readiness")
            try:
                self.identity=nats_info(self.port,self.owner)
                return self.receipt()
            except (OSError,TimeoutError):
                time.sleep(0.1)
        raise TimeoutError("owned NATS broker did not become ready")

    def receipt(self) -> dict:
        return {"kind":"native-windows-process","binary":self.binary,
            "pid":self.process.pid,"owner":self.owner,"identity":self.identity,
            "loopback_port":self.port,"url":f"nats://127.0.0.1:{self.port}"}

    def verify(self) -> None:
        if self.process is None or self.process.poll() is not None:
            raise RuntimeError("owned NATS process is no longer running")
        if nats_info(self.port,self.owner)!=self.identity:
            raise RuntimeError("owned NATS identity changed during verification")

    def close(self) -> dict:
        try:
            if self.process is not None:
                if self.process.poll() is None:
                    self.process.terminate()
                self.process.wait(timeout=10)
                return {"stopped":True,"pid":self.process.pid,
                    "exit_code":self.process.returncode,"method":"retained-owned-process-handle"}
            return {"stopped":True,"not_started":True}
        finally:
            if self.log is not None:
                self.log.close()
