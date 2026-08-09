# Authority-store-isolated execution service. The image contains no management
# CLI, database client, migration entrypoint, or Control sealing material.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends bubblewrap ca-certificates libssl3 python3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m worker \
    && mkdir -p /var/lib/awaken /workspace /outputs \
    && chown -R worker:worker /var/lib/awaken /workspace /outputs

ARG BIN=awaken-worker
COPY ${BIN} /usr/local/bin/awaken-worker

USER 10001
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/awaken-worker"]
