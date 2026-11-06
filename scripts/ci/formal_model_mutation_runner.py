"""The single TLC runner and semantic oracle shared by model mutation families."""

from __future__ import annotations

from pathlib import Path
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[2]


def check_tlc_model_mutations(tlc_jar: Path, name: str, source: str, config: str,
                              mutations: list[tuple[str, str, str]], *,
                              dependencies: tuple[str, ...] = (),
                              heap_mb: int = 512, timeout_seconds: int = 60,
                              workers: int = 1) -> None:
    # One TLC invocation/oracle for every model mutation family. Validate both
    # the process result and semantic outcome; always establish the green base.
    cases = [(source, config, None), *mutations]
    for model_text, config_text, invariant in cases:
        with tempfile.TemporaryDirectory(prefix="awaken-tlc-mutant-") as temp:
            directory = Path(temp)
            for dependency in dependencies:
                shutil.copy2(ROOT / f"formal/tla/{dependency}.tla", directory)
            model = directory / f"{name}.tla"
            model.write_text(model_text, encoding="utf-8")
            config_path = directory / f"{name}.cfg"
            config_path.write_text(config_text, encoding="utf-8")
            # Pinned TLC extracts standard modules by name and deletes them on
            # JVM exit. -metadir isolates states, not those parser files; reuse
            # this invocation's owner for JVM temporaries as well. A timeout or
            # missing tool escapes as an error, never as a semantic kill.
            command = ["java", f"-Djava.io.tmpdir={directory}", f"-Xmx{heap_mb}m", "-XX:+UseParallelGC", "-jar", str(tlc_jar.resolve()),
                       "-workers", str(workers), "-metadir", str(directory / "states"), "-config", str(config_path), str(model)]
            result = subprocess.run(
                command,
                cwd=directory, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                timeout=timeout_seconds, check=False,
            )
            temporal = invariant is not None and invariant.startswith("temporal:")
            action = invariant is not None and invariant.startswith("action:")
            if temporal:
                # TLC does not name a failed temporal property in its output.
                # Restrict this oracle to one explicitly named CFG property.
                properties = [line.strip() for line in config_text.splitlines()
                              if line.strip().startswith(("PROPERTY ", "PROPERTIES "))]
                if properties != [f"PROPERTIES {invariant.removeprefix('temporal:')}"]:
                    raise SystemExit("temporal mutation requires one exact named property")
            expected_code = 0 if invariant is None else (13 if temporal or action else 12)
            expected_text = ("Model checking completed. No error has been found." if invariant is None
                             else f"Action property {invariant.removeprefix('action:')} is violated." if action
                             else "Temporal properties were violated." if temporal
                             else f"Invariant {invariant} is violated")
            if result.returncode != expected_code or expected_text not in result.stdout:
                raise SystemExit(f"{name} {'baseline failed' if invariant is None else f'mutant for {invariant} was not killed correctly'} (exit={result.returncode}):\n{result.stdout}")
