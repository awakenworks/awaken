"""One native executable boundary for private Windows command fixtures."""
from __future__ import annotations

import atexit
from functools import cache
from pathlib import Path
import os
import shlex
import subprocess
import tempfile

from _native_shell import RUST_WINDOWS_SHELL_ARGUMENT


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
    # Share the suite's compiled tool across workers. Each fixture copies it
    # before delegating argv to its own Python or Bash script.
    target = os.environ.get("CARGO_TARGET_DIR")
    parent = Path(target).resolve() / "debug" / "deps" if target else None
    if parent is not None:
        parent.mkdir(parents=True, exist_ok=True)
    directory = tempfile.TemporaryDirectory(prefix="awaken-native-test-tool-", dir=parent)
    atexit.register(directory.cleanup)
    source = Path(directory.name) / "fixture.rs"
    executable = source.with_suffix(".exe")
    source.write_text(RUST_WINDOWS_SHELL_ARGUMENT + '''use std::{env, process::{Command, exit}};
fn main() {
    let program = env::var_os("AWAKEN_FIXTURE_PROGRAM").expect("fixture program");
    let script = env::var_os("AWAKEN_FIXTURE_SCRIPT").expect("fixture script");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let name = std::path::Path::new(&program).file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if matches!(name.as_str(), "bash" | "bash.exe" | "sh" | "sh.exe") {
            let mut command = Command::new(&program);
            command.raw_arg(windows_shell_argument(&script));
            for argument in env::args_os().skip(1) {
                command.raw_arg(windows_shell_argument(&argument));
            }
            let status = command.status().expect("fixture native shell");
            exit(status.code().unwrap_or(1));
        }
    }
    let status = Command::new(program).arg(script)
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
