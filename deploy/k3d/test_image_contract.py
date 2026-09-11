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
    install = dockerfile.split("COPY --chmod=0755 awaken-server", 1)[0]
    for package in ("bubblewrap", "ca-certificates", "libssl3", "python3"):
        assert package in install, f"K3D product image is missing runtime package: {package}"
    mkdir = next(line for line in install.splitlines() if "mkdir -p" in line)
    for mount_point in ("/workspace", "/outputs"):
        assert mount_point in mkdir, f"K3D product image is missing mount point: {mount_point}"
    # I3 migrations, Coordinator and Worker are separate production deployables ->
    # the combined black-box fixture image must copy those exact artifacts, not
    # route any role through the management CLI auxiliary slot. The long-running
    # Control Pod intentionally uses the scenario adapter asserted by I4 below so
    # only its model-publication dependency is deterministic.
    assert "ARG AUXILIARY_EXECUTABLE=awaken-control" in dockerfile
    assert "COPY --chmod=0755 ${AUXILIARY_EXECUTABLE} /usr/local/bin/${AUXILIARY_EXECUTABLE}" in dockerfile
    assert "COPY --chmod=0755 awaken-coordinator /usr/local/bin/awaken-coordinator" in dockerfile
    assert "COPY --chmod=0755 awaken-worker /usr/local/bin/awaken-worker" in dockerfile
    # Image-permission cause/effect table: C1 host artifacts are 0700 under
    # umask 077 or 0755 under 022; C2 COPY installs root-owned executables;
    # C3 a role runs as UID 10001. P1 C1+C2+C3 => execution must remain allowed;
    # P2 either input mode => runtime bytes unchanged and no group/other write.
    # Each image owns the executable mode at COPY, independently of build-host
    # policy. Static coverage locks every shared/production entry path; the live
    # container probe supplies restrictive inputs and executes as UID 10001.
    image_entries = [(Path(__file__).parent / "Dockerfile.server", "awaken-server")] + [
        (Path(__file__).parent.parent / "images" / role / "Dockerfile", f"awaken-{role}")
        for role in ("worker", "control", "coordinator", "sandbox")
    ]
    for image_path, executable in image_entries:
        copies = [line for line in image_path.read_text().splitlines()
                  if line.startswith("COPY ")
                  and line.endswith(f" /usr/local/bin/{executable}")]
        assert len(copies) == 1, image_path
        assert "--chmod=0755" in copies[0].split(), image_path
    resources = (Path(__file__).parent / "distributed-control" / "resources.yaml").read_text()
    policy_graph = (
        Path(__file__).parent / "bases" / "sandbox-network-policy" / "resources.yaml"
    ).read_text()

    def policy(name: str) -> str:
        return policy_graph.split(f"name: {name}", 1)[1].split("---", 1)[0]

    # Network-policy cause/effect rules: N1 exact deny plus open-only widening
    # is the one graph attested by K8sRuntime; generic DNS must not select
    # Sandbox Pods; each production overlay imports that graph and grants only
    # read-only observation to its runtime ServiceAccount.
    assert "ingress: []" in policy("awaken-sandbox-default-deny")
    assert "egress: []" in policy("awaken-sandbox-default-deny")
    assert "awaken-egress: open" in policy("awaken-sandbox-open-egress")
    distributed = Path(__file__).parent / "distributed-control"
    product = Path(__file__).parent / "product-backend"
    dns = (distributed / "network-policies.yaml").read_text()
    assert "operator: NotIn, values: [awaken-sandbox]" in dns
    assert 'resources: ["networkpolicies"]' in resources
    assert 'verbs: ["get", "list", "watch"]' in resources
    for overlay in (distributed, product):
        assert "../bases/sandbox-network-policy" in (overlay / "kustomization.yaml").read_text()
    product_resources = (product / "resources.yaml").read_text()
    assert 'resources: ["networkpolicies"]' in product_resources
    assert 'verbs: ["get", "list", "watch"]' in product_resources
    # Continuation-volume/RBAC cause-effect decision table. C1 a topology runs
    # retained Kubernetes Sessions; C2 its runtime selects the canonical PVC;
    # C3 it can allocate, observe by UID, and finally dispose that claim.
    # Effects: E1 C1+C2+C3 grants only create/get/delete; E2 a missing policy or
    # permission rejects before a recoverable Session can be claimed. Both
    # distributed and all-in-one product topologies reuse the same runtime
    # policy and Kubernetes API contract; neither owns a parallel PVC registry.
    # KRB1=C1+C2+C3=>E1; KRB2=C1+(!C2|!C3)=>E2. Pod update remains a separate
    # rule and must not widen PVCs.
    import tomllib
    import textwrap
    worker = resources.split("  worker.toml: |\n", 1)[1].split("---", 1)[0]
    assert tomllib.loads(textwrap.dedent(worker))["k8s_continuation_volume"] == {
        "storage_class_name": "local-path", "size": "1Gi",
    }
    assert 'resources: ["persistentvolumeclaims"]' in resources
    assert 'verbs: ["create", "get", "delete"]' in resources
    product_config = product_resources.split("  config.toml: |\n", 1)[1].split("---", 1)[0]
    assert tomllib.loads(textwrap.dedent(product_config))["k8s_continuation_volume"] == {
        "storage_class_name": "local-path", "size": "1Gi",
    }
    assert 'resources: ["persistentvolumeclaims"]' in product_resources
    assert 'verbs: ["create", "get", "delete"]' in product_resources
    assert '/usr/local/bin/awaken-control", "database", "migrate"' in resources
    assert 'command: ["/usr/local/bin/awaken-server"]' in resources
    assert '/usr/local/bin/awaken-coordinator", "database", "migrate"' in resources
    assert '/usr/local/bin/awaken-coordinator", "--config"' in resources
    assert '/usr/local/bin/awaken", "coordinator"' not in resources
    assert '/usr/local/bin/awaken", "database", "migrate"' not in resources
    # Installation-intent decision table: DB1 a repeatable rollout manifest ->
    # ordinary exact-only migration and no retained initialization authority;
    # DB2 a fresh disposable E2E cluster -> one explicit audited initialization
    # Job per role, then the same ordinary Jobs converge; DB3 PostgreSQL mode
    # carries the configured expected Workspace directly into its one database
    # binding and never consumes the embedded-only platform-workspace-id marker.
    # Effects: E1 rollout manifests retain no initialization grant; E2 both
    # phases use the same PostgreSQL coordinate without a parallel ConfigMap or
    # Pod-local identity. The fixture helper reuses the role commands and is not
    # a second deployment manifest.
    assert "--initialize-installation" not in resources
    assert resources.count("suspend: true") == 2
    e2e = (
        Path(__file__).parent.parent.parent / "e2e" / "k3d" / "distributed_control_e2e.sh"
    ).read_text()
    # KRB4: the installed provisioner has a shared all-node directory, and K3s
    # may default to local volumes. The node-recovery scenario must select hostPath
    # explicitly; its live Bound/no-node-affinity checks own Kubernetes admission.
    assert "kubectl annotate storageclass local-path defaultVolumeType=hostPath --overwrite" in e2e
    assert "initialize_role_installation control" in e2e
    assert "initialize_role_installation coordinator" in e2e
    assert '"--initialize-installation", "--initialization-reference"' in e2e
    assert 'patch job/control-migrate' in e2e
    assert 'patch job/coordinator-migrate' in e2e
    assert "platform-workspace-id" not in resources
    assert "adr71-installation" not in resources
    assert "platform-workspace-id" not in e2e
    assert "adr71-installation" not in e2e
    # I4 the deterministic Control fixture changes only the publication resolver:
    # the scenario adapter consumes the deployment's one explicit Control config
    # and assembles the canonical split-Control application. The former HOME
    # config mirror would make the command and mounted source disagree.
    assert "AWAKEN_MODEL_MODE, value: distributed-control" in resources
    assert "AWAKEN_SCENARIO_CONFIG, value: /etc/awaken/control.toml" in resources
    assert "/etc/awaken-home" not in resources
    worker_image = (
        Path(__file__).parent.parent / "images" / "worker" / "Dockerfile"
    ).read_text()
    # I4 the standalone production image owns only the Worker artifact, runs as
    # the fixed non-root identity, and carries the Namespace sandbox runtime.
    assert "COPY --chmod=0755 ${BIN} /usr/local/bin/awaken-worker" in worker_image
    assert 'ENTRYPOINT ["/usr/local/bin/awaken-worker"]' in worker_image
    assert "USER 10001" in worker_image
    assert "bubblewrap" in worker_image
    assert "/usr/local/bin/awaken\n" not in worker_image
    # Cause/effect rule I5: selecting a Control or Coordinator production image
    # yields exactly its role-named entry executable; the container cannot switch
    # authority by supplying another aggregate CLI subcommand.
    for role in ("control", "coordinator"):
        role_image = (
            Path(__file__).parent.parent / "images" / role / "Dockerfile"
        ).read_text()
        assert f"COPY --chmod=0755 ${{BIN}} /usr/local/bin/awaken-{role}" in role_image
        assert f'ENTRYPOINT ["/usr/local/bin/awaken-{role}"]' in role_image
        assert "USER 10001" in role_image
        assert " /usr/local/bin/awaken\n" not in role_image
    # Cause/effect decision table. C5 Docker uses a non-default Buildx driver;
    # C6 the production-image acceptance command immediately runs the tagged
    # image. E3 the build explicitly loads its result into Docker's image store.
    # Rule I5: C5+C6 => E3; Podman keeps its ordinary engine-owned output path.
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
    # C7 the catalog changes package/argv/auth/version facts; C8 the Node runtime
    # matrix consumes ids/version probes. E4 installer/verifier consume all
    # generated requirements/executables; E5 Node executes the same generator
    # and owns no literal ids or version table. Constraints: neither a checked-in
    # JSON file nor a language-specific mirror may become another catalog.
    # Decision rules: I6 C7=>E4; I7 C7+C8=>E5. Exact named build-context,
    # fingerprint and rebuild behavior belong solely to the executable
    # e2e/fixtures/sandbox_image.test.mjs contract, not this static image test.
    sandbox = Path(__file__).parent.parent / "images" / "sandbox"
    assert "--example image_runtime_contract" in sandbox_build
    assert not (sandbox / "acp-runtimes.json").exists()
    runtime_profiles = (
        Path(__file__).parent.parent.parent / "e2e" / "acp_runtime_profiles.mjs"
    ).read_text()
    assert "--example', 'image_runtime_contract'" in runtime_profiles
    assert "ACP_RUNTIME_IDS = Object.freeze([" not in runtime_profiles
    assert "ACP_RUNTIME_VERSION_SPECS = Object.freeze({" not in runtime_profiles
    installer = (sandbox / "install-acp-runtimes.py").read_text()
    verifier = (sandbox / "verify-acp-runtimes.py").read_text()
    for source in (installer, verifier):
        assert 'runtime["executables"]' in source
    assert 'runtime["requirements"]' in installer
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


if __name__ == "__main__":
    main()
