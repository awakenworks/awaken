# Awaken Control service image. The image carries one role-named executable;
# configuration selects adapters but cannot switch the process to Coordinator.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m awaken \
    && mkdir -p /var/lib/awaken \
    && chown -R awaken:awaken /var/lib/awaken

ARG BIN=awaken-control
COPY ${BIN} /usr/local/bin/awaken-control

USER 10001
WORKDIR /var/lib/awaken
ENTRYPOINT ["/usr/local/bin/awaken-control"]
