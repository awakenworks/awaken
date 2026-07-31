# Runtime image for K3D scenarios that need the scenario host plus one auxiliary
# executable. The auxiliary executable defaults to the hand binary used by the
# ADR-0044/0045 topologies; production-role scenarios select the shipped CLI.
#
# TWO binaries, TWO planes (the control/execution split):
#   - BRAIN (control plane, the ENTRYPOINT `awaken-server`): the managed HTTP surface
#     whose runs route every tool call to the hand over the cluster network. Selected
#     by AWAKEN_MODEL_MODE=remote-hand + AWAKEN_REMOTE_HAND=hand:9000.
#   - AUXILIARY (by default `awaken-sandbox hand`): the neutral tool-execution
#     endpoint. Hand Pods OVERRIDE the command to run it (listen / dial / nats),
#     e.g. `command: [/usr/local/bin/awaken-sandbox, hand, --listen, 0.0.0.0:9000]`.
#
# Both binaries are built ON THE HOST (rustc 1.96) and copied in — a fast copy, no
# in-container Rust toolchain. ubuntu:24.04 matches the host glibc 2.39 and provides
# libssl3 the server binary links; `sh` for the hand's bash tool.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends bubblewrap ca-certificates libssl3 python3 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/awaken/control /var/lib/awaken/coordinator /workspace /outputs

# The control-plane (brain) binary — the scenario host serving remote-hand mode.
COPY awaken-server /usr/local/bin/awaken-server
# Scenario-specific auxiliary executable. Callers that override the default
# must provide a same-named host-built file in the build context.
ARG AUXILIARY_EXECUTABLE=awaken-sandbox
COPY ${AUXILIARY_EXECUTABLE} /usr/local/bin/${AUXILIARY_EXECUTABLE}

ENTRYPOINT ["/usr/local/bin/awaken-server"]
