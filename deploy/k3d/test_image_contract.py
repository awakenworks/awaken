#!/usr/bin/env python3
"""Static contract for the combined K3D product runtime image."""

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
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


if __name__ == "__main__":
    main()
