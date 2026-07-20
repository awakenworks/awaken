# awaken-sandbox — the EXECUTION-plane image (the sandbox side, opposite the control
# plane). It runs as the process-as-container agent: the ENTRYPOINT is the
# `awaken-sandbox acp` stdio<->TCP bridge, and the container `Cmd` is the ACP CLI argv
# (`command_of(spec)`, e.g. `npx -y @agentclientprotocol/claude-agent-acp@0.44`). The
# bridge spawns that CLI and pipes its stdio to the port the host dials (8080).
#
# ALL supported ACP CLIs are PRE-BAKED (never pulled at runtime): an egress-fenced pod
# (network=None/Allowlist) cannot `npm install`, so the adapter versions MUST be baked
# here and MUST match the MAJOR.MINOR pinned in
# crates/server/awaken-run-executor-acp/src/acp_cli.rs (claude 0.44, codex 1.1).
#
# Build (binary is built ON THE HOST, then staged into the Docker context):
#   deploy/images/sandbox/build.sh ghcr.io/awaken/sandbox:<ver>
# The bridge binary is built on the worker host and copied in below. Trixie's glibc
# is new enough for binaries built on current Linux distributions (including Ubuntu
# 24.04/glibc 2.39); bookworm's 2.36 made the container exit before opening ACP.
FROM node:22-trixie-slim

# Agent tooling most CLIs assume on PATH (git for repo work, rg for search).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git ripgrep tini \
    && rm -rf /var/lib/apt/lists/*

# Pre-bake the ACP adapters + native CLIs. Pin every version (no @latest) so an
# egress-fenced launch is reproducible and the wire codec is a known quantity. Keep
# these in lockstep with acp_cli.rs. A worker image may deliberately narrow this list
# (for example a Codex-only pool) through the build arg; the default remains complete.
ARG ACP_NPM_PACKAGES="@agentclientprotocol/claude-agent-acp@0.44 @agentclientprotocol/codex-acp@1.1 @google/gemini-cli@0.11 opencode-ai@0.6"
RUN npm install -g --no-audit --no-fund ${ACP_NPM_PACKAGES} \
    && npm cache clean --force

# The execution-plane binary (host-built) as the bridge entrypoint.
ARG BIN=deploy/images/sandbox/.awaken-sandbox.bin
COPY ${BIN} /usr/local/bin/awaken-sandbox

# Non-root; the workspace + outputs are tmpfs mounts the pod spec provides.
RUN useradd -u 10001 -m agent \
    && mkdir -p /workspace /mnt/session/outputs \
    && chown -R agent /workspace /mnt/session/outputs
USER 10001
WORKDIR /workspace

# tini reaps the CLI's grandchildren (npx -> node -> …) so a torn-down pod leaves no
# zombies. The bridge listens on 8080; the CLI argv arrives as the container Cmd.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/awaken-sandbox", "acp", "--listen", "0.0.0.0:8080"]
