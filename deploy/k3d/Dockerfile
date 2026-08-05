# Runtime image for K3D scenarios that need the scenario host plus one auxiliary
# executable. Production-role scenarios select the shipped CLI as the auxiliary;
# execution itself remains Worker-owned and each Hand is derived from its realized
# Session Environment (ADR-0073).
#
# Both binaries are built ON THE HOST (rustc 1.96) and copied in — a fast copy, no
# in-container Rust toolchain. ubuntu:24.04 matches the host glibc 2.39 and provides
# libssl3 the server binary links; `sh` for the hand's bash tool.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends bubblewrap ca-certificates libssl3 python3 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/awaken/control /var/lib/awaken/coordinator /workspace /outputs

# Deterministic scenario HTTP surface used by the focused durability fixtures.
COPY awaken-server /usr/local/bin/awaken-server
# Scenario-specific auxiliary executable. Callers that override the default
# must provide a same-named host-built file in the build context.
ARG AUXILIARY_EXECUTABLE=awaken-sandbox
COPY ${AUXILIARY_EXECUTABLE} /usr/local/bin/${AUXILIARY_EXECUTABLE}

ENTRYPOINT ["/usr/local/bin/awaken-server"]
