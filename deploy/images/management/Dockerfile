# Awaken Management image. The Cloud chart owns deployment composition while
# this open image owns the exact CLI artifact and command surface.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

ARG BIN=awaken
COPY ${BIN} /usr/local/bin/awaken

ENTRYPOINT ["/usr/local/bin/awaken"]
