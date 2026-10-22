"""Digest known compilation inputs; Cargo still owns configuration and rebuilds.

Preparation never selects a cached executable. Both listing and execution call
Cargo again. These facts detect known drift; they are not a hermetic toolchain
proof and must never reject valid Cargo configuration by interpreting it here.
"""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import shutil

from crate_test import COMPILE_ENVIRONMENT_OVERRIDES


def _digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _file(path: Path) -> dict:
    return {"path": str(path.resolve()), "sha256": _digest(path.read_bytes()) if path.is_file() else None}


def build_context(root: Path, arguments: list[str], expected: dict | None = None) -> dict:
    """Bind known inputs without executing tools or duplicating Cargo parsing."""
    values = {name.upper() if os.name == "nt" else name: value for name,value in os.environ.items()}
    names = set(COMPILE_ENVIRONMENT_OVERRIDES) | {
        "CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTUP_TOOLCHAIN", "RUSTUP_HOME",
        "RUSTC_BOOTSTRAP", "RUSTDOC", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS",
        "HOME", "USERPROFILE", "CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "ARFLAGS",
        "CL", "_CL_", "LINK", "_LINK_", "LIB", "LIBPATH", "INCLUDE", "EXTERNAL_INCLUDE",
        "VCINSTALLDIR", "VSINSTALLDIR", "VCTOOLSINSTALLDIR", "VCTOOLSVERSION",
        "WINDOWSSDKDIR", "WINDOWSSDKVERSION", "UNIVERSALCRTSDKDIR", "UCRTVERSION",
        "VSCMD_ARG_HOST_ARCH", "VSCMD_ARG_TGT_ARCH", "VSCMD_VER", "RC", "WINDRES",
    }
    names.update(name for name in values if name.startswith(("CARGO_BUILD_", "CARGO_TARGET_", "CARGO_PROFILE_", "CC_", "CXX_", "CFLAGS_", "CXXFLAGS_", "AR_")))
    environment = {name: _digest(values[name].encode()) for name in sorted(names) if name in values}
    def home(name, fallback):
        path = Path(values.get(name, str(fallback)))
        return path if path.is_absolute() else root/path
    parents = (root.resolve(), *root.resolve().parents)
    configs = {parent/".cargo"/name for parent in parents for name in ("config", "config.toml")}
    configs.update(home("CARGO_HOME", Path.home()/".cargo")/name for name in ("config", "config.toml"))
    configs.add(home("RUSTUP_HOME", Path.home()/".rustup")/"settings.toml")
    configs.update(parent/name for parent in parents for name in ("rust-toolchain", "rust-toolchain.toml"))
    before = arguments[:arguments.index("--")] if "--" in arguments else arguments
    for index, arg in enumerate(before):
        value = before[index+1] if arg == "--config" and index+1 < len(before) else arg.partition("=")[2] if arg.startswith("--config=") else None
        if value is not None:
            path = Path(value); path = path if path.is_absolute() else root/path
            # Inline configuration is bound by literal argv. External files of
            # any name are bound by bytes, without interpreting their contents.
            if "=" not in value or path.is_file(): configs.add(path)
    files = [_file(path) for path in sorted(configs)]
    frontends = ("cargo", "rustc", "rustup", *(("cl", "link", "lib", "rc", "mt") if os.name == "nt" else ("cc", "c++", "ar", "ld")))
    tools = {}
    for name in (*frontends, "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                 "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"):
        program = name if name in frontends else values.get(name)
        resolved = shutil.which(program, path=values.get("PATH")) if program else None
        tools[name] = _file(Path(resolved)) if resolved else None
    context = {"kind": "known-compilation-inputs", "environment": environment, "files": files, "tools": tools}
    if expected is not None:
        if expected.get("environment") != environment or expected.get("files") != files:
            raise ValueError("Required build compilation environment/configuration changed")
        if context != expected:
            raise ValueError("Required build compiler/tool identity changed")
    return context
