# Awaken Coordinator service image. Runtime execution is delegated to the
# separately deployed awaken-worker artifact.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m awaken \
    && mkdir -p /var/lib/awaken \
    && chown -R awaken:awaken /var/lib/awaken

ARG BIN=awaken-coordinator
COPY ${BIN} /usr/local/bin/awaken-coordinator

USER 10001
WORKDIR /var/lib/awaken
ENTRYPOINT ["/usr/local/bin/awaken-coordinator"]
