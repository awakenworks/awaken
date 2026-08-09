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
# The bridge binary is built on the worker host and copied in below. Trixie's glibc
# is new enough for binaries built on current Linux distributions (including Ubuntu
# 24.04/glibc 2.39); bookworm's 2.36 made the container exit before opening ACP.
FROM node:22-trixie-slim

# Agent tooling most CLIs assume on PATH (curl for service readiness, git for
# repo work, rg for search), plus the six package-manager runtimes supported by
# Managed Environment provisioning.
# Python packages live in an image-owned virtual environment so pip never mutates
# Debian's externally-managed system interpreter.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates cargo curl git golang-go python3 python3-venv ripgrep ruby tini \
    && python3 -m venv /opt/awaken-python \
    && rm -rf /var/lib/apt/lists/*
ENV HOME="/home/agent" \
    PATH="/opt/awaken-python/bin:${PATH}"

# The E2E harness checks this capability label before reusing a locally cached
# base image, preventing an older image without the package runtimes from being
# mistaken for a complete Environment base.
LABEL org.awaken.environment-packages="1" \
      org.awaken.acp-runtime-contract="1"

# Pre-bake every catalogued ACP adapter/native CLI from one immutable contract.
# `all` is the production default; a deliberately specialized Worker pool may
# pass a comma-separated subset, while an empty value creates a transport-only
# hermetic test fixture. No Session startup installs or downloads an Agent.
ARG ACP_RUNTIME_IDS=all
COPY deploy/images/sandbox/acp-runtimes.json /usr/local/share/awaken/acp-runtimes.json
COPY deploy/images/sandbox/install-acp-runtimes.py /tmp/install-acp-runtimes.py
RUN /usr/bin/python3 /tmp/install-acp-runtimes.py \
      /usr/local/share/awaken/acp-runtimes.json "${ACP_RUNTIME_IDS}" \
    && rm -f /tmp/install-acp-runtimes.py \
    && npm cache clean --force
COPY deploy/images/sandbox/hermes-acp-bootstrap.py /tmp/hermes-acp-bootstrap.py
RUN if [ -x /opt/awaken-python/bin/hermes-acp ]; then \
      mv /opt/awaken-python/bin/hermes-acp /opt/awaken-python/bin/hermes-acp.real \
      && install -m 0755 /tmp/hermes-acp-bootstrap.py /opt/awaken-python/bin/hermes-acp \
      && /opt/awaken-python/bin/hermes-acp --self-test; \
    fi \
    && rm -f /tmp/hermes-acp-bootstrap.py
COPY deploy/images/sandbox/verify-acp-runtimes.py /usr/local/bin/awaken-verify-acp-runtimes
RUN chmod 0755 /usr/local/bin/awaken-verify-acp-runtimes

# The execution-plane binary (host-built with `hand`) supplies both the ACP bridge
# entrypoint and the Session's attached `hand --stdio` exec process.
ARG BIN=deploy/images/sandbox/.awaken-sandbox.bin
COPY ${BIN} /usr/local/bin/awaken-sandbox

# Non-root; the workspace + outputs are tmpfs mounts the pod spec provides.
RUN useradd -u 10001 -m agent \
    && mkdir -p /workspace /mnt/session/outputs \
    && chown -R agent /home/agent /workspace /mnt/session/outputs
USER 10001
WORKDIR /workspace

# tini reaps the CLI's grandchildren (npx -> node -> …) so a torn-down pod leaves no
# zombies. The bridge listens on 8080; the CLI argv arrives as the container Cmd.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/awaken-sandbox", "acp", "--listen", "0.0.0.0:8080"]
