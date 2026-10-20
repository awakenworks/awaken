"""One native executable boundary for private Windows command fixtures."""
from __future__ import annotations

import atexit
from functools import cache
from pathlib import Path
import os
import shlex
import subprocess
import tempfile


def python_script_header(interpreter: str | Path) -> str:
    """Keep spaces and quotes in the real interpreter path across shebang parsing."""
    return f"#!/usr/bin/env -S {shlex.quote(Path(interpreter).as_posix())}\n"


def compiler_command(environment: dict[str, str]) -> list[str]:
    compiler = environment.get("RUSTC") or "rustc"
    wrapper = environment.get("RUSTC_WRAPPER")
    return [wrapper, compiler] if wrapper else [compiler]


@cache
def native_fixture_launcher() -> Path:
    # Bare CreateProcess command lookup bypasses PATH-first batch/shell files.
    # Compile once; every fixture delegates its original argv to its chosen
    # Python or Bash script. The tests already read actual rustc identity.
    target = os.environ.get("CARGO_TARGET_DIR")
    parent = Path(target).resolve() / "debug" / "deps" if target else None
    if parent is not None:
        parent.mkdir(parents=True, exist_ok=True)
    directory = tempfile.TemporaryDirectory(prefix="awaken-native-test-tool-", dir=parent)
    atexit.register(directory.cleanup)
    source = Path(directory.name) / "fixture.rs"
    executable = source.with_suffix(".exe")
    source.write_text('''use std::{env, process::{Command, exit}};
fn main() {
    let status = Command::new(env::var_os("AWAKEN_FIXTURE_PROGRAM").expect("fixture program"))
        .arg(env::var_os("AWAKEN_FIXTURE_SCRIPT").expect("fixture script"))
        .args(env::args_os().skip(1)).status().expect("fixture subprocess");
    exit(status.code().unwrap_or(1));
}
''', encoding="utf-8")
    result = subprocess.run([*compiler_command(dict(os.environ)), "--crate-name", "native_command_fixture", "--edition=2021",
                             str(source), "-o", str(executable)], stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, encoding="utf-8")
    if result.returncode:
        raise RuntimeError("native command fixture compilation failed:\n" + result.stdout)
    return executable
