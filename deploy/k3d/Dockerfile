# Runtime image for a brain/hand network-topology scenario (ADR-0044/0045).
#
# TWO binaries, TWO planes (the control/execution split):
#   - BRAIN (control plane, the ENTRYPOINT `awaken-server`): the managed HTTP surface
#     whose runs route every tool call to the hand over the cluster network. Selected
#     by AWAKEN_MODEL_MODE=remote-hand + AWAKEN_REMOTE_HAND=hand:9000.
#   - HAND  (execution plane, `awaken-sandbox hand`): the neutral tool-execution
#     endpoint. Hand Pods OVERRIDE the command to run it (listen / dial / nats),
#     e.g. `command: [/usr/local/bin/awaken-sandbox, hand, --listen, 0.0.0.0:9000]`.
#
# Both binaries are built ON THE HOST (rustc 1.96) and copied in — a fast copy, no
# in-container Rust toolchain. ubuntu:24.04 matches the host glibc 2.39 and provides
# libssl3 the server binary links; `sh` for the hand's bash tool.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

# The control-plane (brain) binary — the scenario host serving remote-hand mode.
COPY awaken-server /usr/local/bin/awaken-server
# The execution-plane (hand) binary — hand Pods override the command to run it.
COPY awaken-sandbox /usr/local/bin/awaken-sandbox

ENTRYPOINT ["/usr/local/bin/awaken-server"]
