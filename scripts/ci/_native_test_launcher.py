"""One native executable boundary for private Windows command fixtures."""
from __future__ import annotations

import atexit
from functools import cache
from pathlib import Path
import subprocess
import tempfile


@cache
def native_fixture_launcher() -> Path:
    # Bare CreateProcess command lookup bypasses PATH-first batch/shell files.
    # Compile once; every fixture delegates its original argv to its chosen
    # Python or Bash script. The tests already read actual rustc identity.
    directory = tempfile.TemporaryDirectory(prefix="awaken-native-test-tool-")
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
    result = subprocess.run(["rustc", "--crate-name", "native_command_fixture", "--edition=2021",
                             str(source), "-o", str(executable)], stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, encoding="utf-8")
    if result.returncode:
        raise RuntimeError("native command fixture compilation failed:\n" + result.stdout)
    return executable
