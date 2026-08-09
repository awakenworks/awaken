#!/usr/bin/env python3
"""Static contract for the combined K3D product runtime image."""

import json
from pathlib import Path


def main() -> None:
    # Cause/effect decision table. C1 the product Pod executes rooted Bash tools;
    # C2 bundled Skills may invoke Python validators; C3 TLS/sandbox primitives
    # remain required; C4 Namespace layout binds workspace and outputs onto image
    # mount points. E1 the one canonical image contains python3, bubblewrap, CA
    # roots and OpenSSL; E2 both mount points exist before either shipped binary is
    # copied. Rules: I1 C1+C2+C3 => E1; I2 C1+C4 => E2.
    dockerfile = (Path(__file__).parent / "Dockerfile").read_text()
    install = dockerfile.split("COPY awaken-server", 1)[0]
    for package in ("bubblewrap", "ca-certificates", "libssl3", "python3"):
        assert package in install, f"K3D product image is missing runtime package: {package}"
    mkdir = next(line for line in install.splitlines() if "mkdir -p" in line)
    for mount_point in ("/workspace", "/outputs"):
        assert mount_point in mkdir, f"K3D product image is missing mount point: {mount_point}"
    # Cause/effect decision table. C5 Docker uses a non-default Buildx driver;
    # C6 the production-image acceptance command immediately runs the tagged
    # image. E3 the build explicitly loads its result into Docker's image store.
    # Rule I3: C5+C6 => E3; Podman keeps its ordinary engine-owned output path.
    sandbox_build = (
        Path(__file__).parent.parent / "images" / "sandbox" / "build.sh"
    ).read_text()
    assert 'if [[ "${engine##*/}" == "docker" ]]' in sandbox_build
    assert (
        'run_with_deadline "$build_timeout_seconds" "$engine" build --load "$@"'
        in sandbox_build
    )
    assert (
        'run_with_deadline "$build_timeout_seconds" "$engine" build "$@"'
        in sandbox_build
    )
    assert sandbox_build.count("  build_image ") == 2
    # C7 an ACP wrapper can be present while the coding CLI it delegates to is
    # absent; C8 a runtime can use more than one package manager requirement.
    # E4 every runtime declares all installed packages and all executables that
    # must survive into the final image. Rule I4: C7+C8 => E4, enforced both at
    # installation and again by the production handshake verifier.
    sandbox = Path(__file__).parent.parent / "images" / "sandbox"
    runtime_contract = json.loads((sandbox / "acp-runtimes.json").read_text())
    runtimes = {runtime["id"]: runtime for runtime in runtime_contract["runtimes"]}
    assert set(runtimes) == {"claude", "codex", "gemini", "opencode", "hermes"}
    for runtime in runtimes.values():
        assert runtime["requirements"], f"{runtime['id']} has no package requirements"
        assert runtime["executables"], f"{runtime['id']} has no executable contract"
        assert all(
            item["manager"] in {"npm", "pip"} and item["requirement"]
            for item in runtime["requirements"]
        )
    assert {"claude-agent-acp", "claude"} <= set(runtimes["claude"]["executables"])
    assert {"codex-acp", "codex"} <= set(runtimes["codex"]["executables"])
    assert {"hermes-acp", "hermes"} <= set(runtimes["hermes"]["executables"])
    installer = (sandbox / "install-acp-runtimes.py").read_text()
    verifier = (sandbox / "verify-acp-runtimes.py").read_text()
    for source in (installer, verifier):
        assert 'runtime["executables"]' in source
    assert 'runtime["requirements"]' in installer
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


if __name__ == "__main__":
    main()
