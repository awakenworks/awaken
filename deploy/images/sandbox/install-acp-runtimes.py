#!/usr/bin/env python3
"""Install the exact ACP runtimes declared by the sandbox image contract."""

import json
import shutil
import subprocess
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("usage: install-acp-runtimes.py CONTRACT all|ID[,ID...]")
    contract = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    runtimes = contract["runtimes"]
    requested = sys.argv[2]
    selected_ids = (
        {runtime["id"] for runtime in runtimes}
        if requested == "all"
        else {item for item in requested.split(",") if item}
    )
    known_ids = {runtime["id"] for runtime in runtimes}
    unknown = sorted(selected_ids - known_ids)
    if unknown:
        raise SystemExit(f"unknown ACP runtime ids: {', '.join(unknown)}")

    selected = [runtime for runtime in runtimes if runtime["id"] in selected_ids]
    npm = [runtime["requirement"] for runtime in selected if runtime["manager"] == "npm"]
    pip = [runtime["requirement"] for runtime in selected if runtime["manager"] == "pip"]
    unsupported = sorted({runtime["manager"] for runtime in selected} - {"npm", "pip"})
    if unsupported:
        raise SystemExit(f"unsupported ACP runtime managers: {', '.join(unsupported)}")
    if npm:
        subprocess.run(["npm", "install", "-g", "--no-audit", "--no-fund", *npm], check=True)
    if pip:
        subprocess.run(["pip", "install", "--no-cache-dir", *pip], check=True)

    for runtime in selected:
        executable = next(
            (part for part in runtime["probe_argv"] if "=" not in part and part != "/usr/bin/env"),
            None,
        )
        if executable is None or shutil.which(executable) is None:
            raise SystemExit(f"{runtime['id']}: installed ACP executable is unavailable")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
