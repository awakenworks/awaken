# Runtime image for a brain/hand network-topology scenario (ADR-0044/0045).
#
# One image, two roles chosen by env at runtime:
#   - HAND  (AWAKEN_HAND_LISTEN=0.0.0.0:9000): serves the neutral executor channel
#     on TCP; a pod running this is a remote execution endpoint.
#   - BRAIN (AWAKEN_MODEL_MODE=remote-hand, AWAKEN_REMOTE_HAND=hand:9000): the
#     managed HTTP surface whose runs route every tool call to the hand pod over
#     the cluster network (the Direct topology).
#
# The binary is built ON THE HOST (rustc 1.96) and copied in, so the image build
# is a fast copy — no in-container Rust toolchain. ubuntu:24.04 matches the host
# glibc 2.39 and provides libssl3/libcrypto3 the binary dynamically links.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY awaken-server-local /usr/local/bin/awaken-server-local

ENTRYPOINT ["/usr/local/bin/awaken-server-local"]
