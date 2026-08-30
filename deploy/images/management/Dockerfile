# Awaken Management image. The same canonical `awaken` binary serves local
# evaluation and production composition. Its entrypoint only projects the
# existing signed Worker credential contract before exec; it adds no command
# parser, lifecycle owner, or parallel product launcher.
FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 awaken \
    && install -d -o awaken -g awaken /var/lib/awaken

ARG BIN=.awaken-management.bin
ARG SOURCE_REPOSITORY=""
ARG SOURCE_REVISION=""
ARG VERSION=""
COPY ${BIN} /usr/local/bin/awaken
COPY container.toml /etc/awaken/config.toml
COPY entrypoint.sh /usr/local/bin/awaken-container-entrypoint

LABEL org.opencontainers.image.source="$SOURCE_REPOSITORY" \
      org.opencontainers.image.revision="$SOURCE_REVISION" \
      org.opencontainers.image.version="$VERSION"

RUN chmod 0755 /usr/local/bin/awaken \
    && chmod 0755 /usr/local/bin/awaken-container-entrypoint \
    && chmod 0644 /etc/awaken/config.toml

USER awaken
EXPOSE 8080
VOLUME ["/var/lib/awaken"]

ENTRYPOINT ["/usr/local/bin/awaken-container-entrypoint"]
CMD ["all-in-one", "--config", "/etc/awaken/config.toml"]
