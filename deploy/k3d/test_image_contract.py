#!/usr/bin/env python3
"""Static contract for the combined K3D product runtime image."""

import argparse
import re
import sys
import textwrap
import tomllib
from pathlib import Path


def main() -> None:
    # Cause/effect decision table. C1 the product Pod executes rooted Bash tools;
    # C2 bundled Skills may invoke Python validators; C3 TLS/sandbox primitives
    # remain required; C4 Namespace layout binds workspace and outputs onto image
    # mount points. E1 the one canonical image contains python3, bubblewrap, CA
    # roots and OpenSSL; E2 both mount points exist before either shipped binary is
    # copied. Rules: I1 C1+C2+C3 => E1; I2 C1+C4 => E2.
    dockerfile = (Path(__file__).parent / "Dockerfile").read_text(encoding="utf-8")
    # I8 a cold/repeated build must not resolve a mutable Ubuntu tag into a
    # different substrate: use the production management image's exact base.
    # Missing pin or divergent base => fail before paid cluster setup. This
    # static contract proves identity, not registry availability or TLS health.
    management = (Path(__file__).parent.parent / "images" / "management" / "Dockerfile").read_text(encoding="utf-8")
    runtime_base = next(line for line in dockerfile.splitlines() if line.startswith("FROM "))
    management_base = next(line for line in management.splitlines() if line.startswith("FROM "))
    assert "@sha256:" in runtime_base
    assert runtime_base == management_base, "K3D must use the production Ubuntu base"
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
        copies = [line for line in image_path.read_text(encoding="utf-8").splitlines()
                  if line.startswith("COPY ")
                  and line.endswith(f" /usr/local/bin/{executable}")]
        assert len(copies) == 1, image_path
        assert "--chmod=0755" in copies[0].split(), image_path
    # TLS bootstrap decision table: C1 the pinned minimal Ubuntu base lacks
    # CA roots; C2 its two official APT URIs use HTTP; C3 an index download fails.
    # T1 C1+C2 -> bootstrap the pinned Rust CA bundle, change transport to HTTPS
    # before update, and keep Ubuntu's existing signed repository identity.
    # T2 C3 -> fail update before installation, never reuse incomplete indexes.
    # The real canonical image build supplies network/TLS acceptance. These
    # static mutants reject each missing boundary without a second source list.
    sandbox_source = (Path(__file__).parent.parent / "images" / "sandbox" / "Dockerfile").read_text(encoding="utf-8")
    ca_copy = "COPY --from=rust-toolchain /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt"
    https_sources = "sed -i 's#^URIs: http://#URIs: https://#' /etc/apt/sources.list.d/ubuntu.sources"
    strict_update = "apt-get -o APT::Update::Error-Mode=any update"

    def tls_bootstrap_contract(text: str) -> bool:
        return all(part in text for part in (ca_copy, https_sources, strict_update)) and (
            text.index(ca_copy) < text.index(https_sources) < text.index(strict_update)
        )

    assert tls_bootstrap_contract(sandbox_source)
    for part in (ca_copy, https_sources, strict_update):
        assert not tls_bootstrap_contract(sandbox_source.replace(part, "", 1))
    resources = (Path(__file__).parent / "distributed-control" / "resources.yaml").read_text(encoding="utf-8")
    policy_graph = (
        Path(__file__).parent / "bases" / "sandbox-network-policy" / "resources.yaml"
    ).read_text(encoding="utf-8")

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
    dns = (distributed / "network-policies.yaml").read_text(encoding="utf-8")
    assert "operator: NotIn, values: [awaken-sandbox]" in dns
    shared_rbac = (
        Path(__file__).parent / "bases" / "sandbox-runtime-rbac" / "resources.yaml"
    ).read_text(encoding="utf-8")
    assert 'resources: ["networkpolicies"]' in shared_rbac
    assert 'verbs: ["get", "list", "watch"]' in shared_rbac
    for overlay in (distributed, product):
        assert "../bases/sandbox-network-policy" in (overlay / "kustomization.yaml").read_text(encoding="utf-8")
    product_resources = (product / "resources.yaml").read_text(encoding="utf-8")
    for overlay in (distributed, product):
        assert "../bases/sandbox-runtime-rbac" in (overlay / "kustomization.yaml").read_text(encoding="utf-8")
    # Worker-retirement cause/effect decision table. C1 Kubernetes begins a
    # normal Job/StatefulSet replacement; C2 the Worker can still claim work
    # until its local gate closes; C3 an in-flight model/tool may outlive the
    # ordinary drain and require the bounded cancellation/return observer; C4
    # Kubernetes sends SIGKILL when its Pod grace expires. Effects: E1 preStop
    # invokes the one existing Worker `/admin/drain` authority before SIGTERM;
    # E2 the Pod grace covers configured drain + revocation + bounded lifecycle
    # publication/withdrawal; E3 no deployment-owned stop protocol is added.
    # C5 the authoritative admin router admits POST only: a Kubernetes httpGet
    # hook receives 405 and never closes admission. Rules WR1=C1+C2+C5=>E1;
    # WR2=C1+C3+C4=>E2; WR3=!E1|!E2=>reject; WR4=GET instead of POST=>reject.
    # The Worker lifecycle tests own runtime drain/cancellation behavior; this
    # test owns the cross-language deployment budget that exposed the crash.
    lifecycle = (
        Path(__file__).parent.parent.parent
        / "crates" / "bin" / "awaken-worker" / "src" / "lifecycle.rs"
    ).read_text(encoding="utf-8")
    revocation_budget = int(re.search(
        r"WORKER_REVOCATION_BUDGET[^=]*=.*from_secs\((\d+)\)", lifecycle,
    ).group(1))

    def worker_retirement_contract(resources_text: str, marker: str) -> bool:
        worker = resources_text.split(marker, 1)[1].split("\n---", 1)[0]
        config = resources_text.split("worker.toml: |\n", 1)[1].split("\n---", 1)[0]
        drain_budget = tomllib.loads(textwrap.dedent(config))["worker_drain_grace_secs"]
        grace = int(re.search(r"terminationGracePeriodSeconds:\s*(\d+)", worker).group(1))
        return (
            "preStop:" in worker
            and "exec:" in worker
            and 'urllib.request.Request("http://127.0.0.1:9090/admin/drain",method="POST")' in worker
            and grace >= drain_budget + revocation_budget + 10
        )

    product_marker = "kind: Job\nmetadata:\n  name: awaken-worker"
    # Persistent Worker recovery cause/effect rules: PR1 a lease/transport loss
    # closes the existing local authority and exits nonzero; PR2 repeated exits
    # exceed the old finite Job backoff; PR3 Kubernetes disrupts a Pod before a
    # container exit is available. Effects: one fresh non-restarting Pod UID is
    # created by the native Job controller; neither PR1 nor PR3 spends the Job's
    # failure budget. Original process evidence, drain and lease gates remain.
    # PR4 missing/Count/In policy => reject this deployment before apply.
    # C5 the installed API schema requires an explicit condition status;
    # E5 the canonical disruption rule declares True for both old and new
    # servers. PR5 omitted/False/Unknown status => reject before apply, rather
    # than disabling kubectl validation or narrowing the retry contract. This
    # shape gate complements the live controller replacement witness; it does
    # not claim that a new Pod or a timer proves old execution has settled.
    def worker_replacement_contract(resources_text: str) -> bool:
        worker = resources_text.split(product_marker, 1)[1].split("\n---", 1)[0]
        return (
            "restartPolicy: Never" in worker
            and "podFailurePolicy:" in worker
            and re.search(r"action: Ignore\s+onExitCodes:\s+containerName: worker\s+operator: NotIn\s+values: \[0\]", worker) is not None
            and re.search(r'action: Ignore\s+onPodConditions:\s+- type: DisruptionTarget\s+status: "True"', worker) is not None
            and "activeDeadlineSeconds:" not in worker
        )

    assert worker_replacement_contract(product_resources), "persistent Worker failures must not exhaust Job retries"
    assert not worker_replacement_contract(product_resources.replace("action: Ignore", "action: Count"))
    assert not worker_replacement_contract(product_resources.replace("operator: NotIn", "operator: In"))
    for status in ('', '            status: "False"', '            status: "Unknown"'):
        assert not worker_replacement_contract(product_resources.replace('            status: "True"', status))
    distributed_marker = "kind: StatefulSet\nmetadata: { name: worker, labels: { app: worker } }"
    # PR6 (ADR-0078 amendment 2026-10-31): the Coordinator adds the Worker
    # process-evidence finalizer at registration, once the Pod can bind. A
    # template finalizer would pin every Pod that never registers, so neither
    # the product Job nor the distributed StatefulSet template may carry it.
    def worker_template_finalizer_free(resources_text: str, marker: str) -> bool:
        worker = resources_text.split(marker, 1)[1].split("\n---", 1)[0]
        return "awaken.dev/worker-process-evidence" not in worker
    assert worker_template_finalizer_free(product_resources, product_marker), "PR6 product"
    assert worker_template_finalizer_free(resources, distributed_marker), "PR6 distributed"
    assert not worker_template_finalizer_free(
        product_resources.replace(
            "        app.kubernetes.io/name: awaken-worker\n      # No process-evidence",
            "        app.kubernetes.io/name: awaken-worker\n      finalizers: [awaken.dev/worker-process-evidence]\n      # No process-evidence",
        ),
        product_marker,
    ), "PR6 mutation: template finalizer"
    assert worker_retirement_contract(product_resources, product_marker)
    assert worker_retirement_contract(resources, distributed_marker)
    assert not worker_retirement_contract(
        product_resources.replace('method="POST"', 'method="GET"'), product_marker,
    )
    # Mutation witnesses for WR3: either the old 20s grace or deleting the hook
    # must fail this exact deployment contract.
    assert not worker_retirement_contract(
        product_resources.replace("terminationGracePeriodSeconds: 60", "terminationGracePeriodSeconds: 20", 1),
        product_marker,
    )
    assert not worker_retirement_contract(
        product_resources.replace("preStop:", "removedPreStop:", 1),
        product_marker,
    )
    # Retained-storage selection stays in this image/config contract. The
    # distributed deployment contract and live can-i table own the separate
    # Coordinator Create and Worker read/dispose permissions; copying those
    # Role rules here would create a second RBAC test authority.
    worker = resources.split("  worker.toml: |\n", 1)[1].split("---", 1)[0]
    assert tomllib.loads(textwrap.dedent(worker))["k8s_continuation_volume"] == {
        "storage_class_name": "local-path", "size": "1Gi",
    }
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
    ).read_text(encoding="utf-8")
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
    ).read_text(encoding="utf-8")
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
        ).read_text(encoding="utf-8")
        assert f"COPY --chmod=0755 ${{BIN}} /usr/local/bin/awaken-{role}" in role_image
        assert f'ENTRYPOINT ["/usr/local/bin/awaken-{role}"]' in role_image
        assert "USER 10001" in role_image
        assert " /usr/local/bin/awaken\n" not in role_image
    # Cause/effect decision table. C5 Docker uses a non-default Buildx driver;
    # C6 the production-image acceptance command immediately runs the tagged
    # image; C7 Docker provenance policy is absent or already validated into
    # the sole build argument array. E3 the build explicitly loads its result
    # into Docker's image store; E4 the exact provenance selection reaches that
    # same build instead of a parallel command. Rules: I5 C5+C6 => E3; I5b
    # C5+C7 => E4. Podman keeps its ordinary engine-owned output path.
    sandbox_build = (
        Path(__file__).parent.parent / "images" / "sandbox" / "build.sh"
    ).read_text(encoding="utf-8")
    assert 'if [[ "${engine##*/}" == "docker" ]]' in sandbox_build
    assert (
        'run_with_deadline "$build_timeout_seconds" "$engine" build --load --platform linux/amd64 \\\n'
        '      "${build_provenance_args[@]}" "$@"'
        in sandbox_build
    )
    assert (
        'run_with_deadline "$build_timeout_seconds" "$engine" build --platform linux/amd64 "$@"'
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
    sandbox_dockerfile = (sandbox / "Dockerfile").read_text(encoding="utf-8")
    # The public Managed Agents foundation is one immutable, observable contract:
    # Ubuntu 24.04/amd64 with Node 22, Go 1.24, and Ruby 3.3 as defaults. Build
    # and reuse admission must test that same projection, while every external
    # base is digest pinned.
    assert "FROM docker.io/library/ubuntu:24.04@sha256:" in sandbox_dockerfile
    assert "FROM docker.io/library/node:22-trixie-slim@sha256:" in sandbox_dockerfile
    assert "FROM docker.io/library/golang:1.24-bookworm@sha256:" in sandbox_dockerfile
    assert "FROM docker.io/library/ruby:3.3-bookworm@sha256:" in sandbox_dockerfile
    assert "ARG RUST_TOOLCHAIN_IMAGE=docker.io/library/rust:1.96-bookworm@sha256:" in sandbox_dockerfile
    assert "FROM ${RUST_TOOLCHAIN_IMAGE} AS rust-toolchain" in sandbox_dockerfile
    assert sandbox_dockerfile.count("@sha256:") == 5
    assert 'org.awaken.managed-agents-base-contract="ubuntu-24.04-amd64-v1"' in sandbox_dockerfile
    assert 'org.awaken.sandbox-role-contract="2"' in sandbox_dockerfile
    assert "build --load --platform linux/amd64" in sandbox_build
    assert "build --platform linux/amd64" in sandbox_build
    assert "accept_managed_agents_base" in sandbox_build
    for role in (
        "control-forwarder",
        "control-forwarder-ready",
        "repository-publish-noreplace",
        "read-tree-nofollow",
        "repository-effect",
    ):
        assert role in sandbox_build
    assert "--example image_runtime_contract" in sandbox_build
    assert not (sandbox / "acp-runtimes.json").exists()
    runtime_profiles = (
        Path(__file__).parent.parent.parent / "e2e" / "acp_runtime_profiles.mjs"
    ).read_text(encoding="utf-8")
    assert "--example', 'image_runtime_contract'" in runtime_profiles
    assert "ACP_RUNTIME_IDS = Object.freeze([" not in runtime_profiles
    assert "ACP_RUNTIME_VERSION_SPECS = Object.freeze({" not in runtime_profiles
    installer = (sandbox / "install-acp-runtimes.py").read_text(encoding="utf-8")
    verifier = (sandbox / "verify-acp-runtimes.py").read_text(encoding="utf-8")
    for source in (installer, verifier):
        assert 'runtime["executables"]' in source
    assert 'runtime["requirements"]' in installer
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="test_image_contract.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="ignored; the image contract is read whole")
    return parser.parse_args(argv)


if __name__ == "__main__":
    parse(sys.argv[1:])
    main()
