# awaken-sandbox — the EXECUTION-plane image (the sandbox side, opposite the control
# plane). It runs as the process-as-container agent: the ENTRYPOINT is the
# `awaken-sandbox acp` stdio<->TCP bridge, and the container `Cmd` is the ACP CLI argv
# (`command_of(spec)`, e.g. `claude-agent-acp`). The
# bridge spawns that CLI and pipes its stdio to the port the host dials (8080).
#
# ALL supported ACP CLIs are PRE-BAKED (never pulled at runtime): an egress-fenced pod
# (network=None/Allowlist) cannot `npm install`, so the adapter versions MUST be baked
# here and MUST match the MAJOR.MINOR pinned in
# crates/server/awaken-run-executor-acp/src/acp_cli.rs.
#
# Build (binary is built ON THE HOST, then staged into the Docker context):
#   deploy/images/sandbox/build.sh ghcr.io/awaken/sandbox:<ver>
# The bridge binary is built by `stage-binary.sh` and copied in below. The final
# rootfs is fixed independently at Ubuntu 24.04; the post-build acceptance starts
# the real binary there, so an incompatible host glibc fails this build instead
# of silently selecting another Sandbox operating system.
# Fully qualified sources are the single Docker/Podman contract: Docker may
# resolve short names implicitly, while a secure Podman host has no ambient
# unqualified-search registry and must still build these exact public images.
ARG RUST_TOOLCHAIN_SERIES=1.96
ARG RUST_TOOLCHAIN_IMAGE=docker.io/library/rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
FROM ${RUST_TOOLCHAIN_IMAGE} AS rust-toolchain
ARG RUST_TOOLCHAIN_SERIES
RUN rustc --version | grep -Eq "^rustc ${RUST_TOOLCHAIN_SERIES}\\." \
    && rustup component add clippy rustfmt

FROM docker.io/library/node:22-trixie-slim@sha256:7b8a0c89c54499bee567618f96578e1a12a800f062fbdbfd1fb6a443fa6f6284 AS node-toolchain
FROM docker.io/library/golang:1.24-bookworm@sha256:1a6d4452c65dea36aac2e2d606b01b4a029ec90cc1ae53890540ce6173ea77ac AS go-toolchain
FROM docker.io/library/ruby:3.3-bookworm@sha256:daf7fa54695b1fcb24fadd7b8cd18ff847e20ce76fe5a908e3cfe9af9147bc46 AS ruby-toolchain

# Anthropic's public Cloud Sandbox reference specifies Ubuntu 24.04 LTS on
# amd64. The canonical build owner supplies `--platform linux/amd64`; the digest
# below prevents the named OS baseline from moving independently of review.
# https://platform.claude.com/docs/en/managed-agents/cloud-sandboxes-reference
FROM docker.io/library/ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517

# Node 22, Go 1.24, and Ruby 3.3 are the documented default Managed Agents
# runtimes. Copy exact official toolchains rather than letting Ubuntu's package
# versions silently redefine that public surface.
COPY --from=node-toolchain /usr/local/bin/ /usr/local/bin/
COPY --from=node-toolchain /usr/local/lib/node_modules/ /usr/local/lib/node_modules/
COPY --from=go-toolchain /usr/local/go/ /usr/local/go/
COPY --from=ruby-toolchain /usr/local/ /usr/local/

# Keep one pinned, complete Rust toolchain in the sandbox base. Debian's `cargo`
# package does not include rustup components such as Clippy, which makes a
# dynamically composed native/desktop Environment unable to honor ordinary
# repository quality gates. Environment layers extend this base with system
# libraries; they must not have to repair its language toolchains.
COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup

# Agent tooling most CLIs assume on PATH (curl for service readiness, git for
# repo work, rg for search), plus the six package-manager runtimes supported by
# Managed Environment provisioning.
# Python packages live in an image-owned virtual environment so pip never mutates
# the distribution's externally-managed system interpreter.
RUN DEBIAN_FRONTEND=noninteractive apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates curl git libgmp10 libyaml-0-2 python3 python3-venv ripgrep tini \
    && python3 -m venv /opt/awaken-python \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_HOME="/usr/local/cargo" \
    HOME="/home/agent" \
    PATH="/usr/local/cargo/bin:/usr/local/go/bin:/opt/awaken-python/bin:${PATH}" \
    RUSTUP_HOME="/usr/local/rustup"

# The E2E harness checks this capability label before reusing a locally cached
# base image, preventing an older image without the package runtimes from being
# mistaken for a complete Environment base.
LABEL org.awaken.environment-packages="3" \
      org.awaken.acp-runtime-contract="1" \
      org.awaken.managed-agents-base-contract="ubuntu-24.04-amd64-v1" \
      org.awaken.sandbox-role-contract="2"
ARG AWAKEN_BUILD_INPUT_FINGERPRINT
LABEL org.awaken.build-input-fingerprint="${AWAKEN_BUILD_INPUT_FINGERPRINT}"

# Pre-bake every catalogued ACP adapter/native CLI from one immutable contract.
# `all` is the production default; a deliberately specialized Worker pool may
# pass a comma-separated subset, while an empty value creates a transport-only
# hermetic test fixture. No Session startup installs or downloads an Agent.
ARG ACP_RUNTIME_IDS=all
COPY --from=awaken_generated .acp-runtimes.generated.json /usr/local/share/awaken/acp-runtimes.json
COPY install-acp-runtimes.py /tmp/install-acp-runtimes.py
RUN /usr/bin/python3 /tmp/install-acp-runtimes.py \
      /usr/local/share/awaken/acp-runtimes.json "${ACP_RUNTIME_IDS}" \
    && rm -f /tmp/install-acp-runtimes.py \
    && npm cache clean --force
COPY hermes-acp-bootstrap.py /tmp/hermes-acp-bootstrap.py
RUN if [ -x /opt/awaken-python/bin/hermes-acp ]; then \
      mv /opt/awaken-python/bin/hermes-acp /opt/awaken-python/bin/hermes-acp.real \
      && install -m 0755 /tmp/hermes-acp-bootstrap.py /opt/awaken-python/bin/hermes-acp \
      && /opt/awaken-python/bin/hermes-acp --self-test; \
    fi \
    && rm -f /tmp/hermes-acp-bootstrap.py
COPY verify-acp-runtimes.py /usr/local/bin/awaken-verify-acp-runtimes
RUN chmod 0755 /usr/local/bin/awaken-verify-acp-runtimes \
    && /usr/local/bin/awaken-verify-acp-runtimes --self-test

# The execution-plane binary (host-built with `hand`) supplies the ACP bridge,
# the Session's attached `hand --stdio` process, the internal atomic Repository
# publisher, and the nofollow terminal tree reader validated by `build.sh`
# before an image is accepted.
COPY --from=awaken_generated --chmod=0755 .awaken-sandbox.bin /usr/local/bin/awaken-sandbox

# Non-root; the workspace + outputs are tmpfs mounts the pod spec provides.
RUN useradd -u 10001 -m agent \
    && mkdir -p /workspace /mnt/session/outputs \
    && chown -R agent /home/agent /workspace /mnt/session/outputs
USER 10001
WORKDIR /workspace

# tini reaps the CLI's grandchildren (npx -> node -> …) so a torn-down pod leaves no
# zombies. The bridge listens on 8080; the CLI argv arrives as the container Cmd.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/awaken-sandbox", "acp", "--listen", "0.0.0.0:8080"]
