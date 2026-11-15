# The MinIO client the PostgreSQL gate and its recovery drill drive.
#
# MinIO no longer publishes images: quay.io/minio/* manifests are gone, the
# Docker Hub repositories refuse pulls and dl.min.io answers 410. The signed
# release binaries remain on GitHub, so this recipe installs the exact pinned
# release binary, verified by its sha256, on a digest-pinned Alpine.
# scripts/ci/pg_tests.sh builds it on first use under a tag whose suffix is the
# first 12 hex digits of the amd64 pin, and refuses a tag that disagrees with
# the pins here; change both together.
FROM alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc
ARG TARGETARCH
ARG RELEASE=RELEASE.2024-11-21T17-21-54Z
ARG SHA256_amd64=0312010a9d0aa7a52b4dfb14330f289ee4e295def80b3a3f530d4f38c1b71da0
ARG SHA256_arm64=a6b0b36ccd4674524674a032d10b1a474ffe4187e93d83f53560092c86c57ac4
RUN set -eu; \
    arch="${TARGETARCH:-$(uname -m)}"; \
    case "$arch" in \
      amd64|x86_64) arch=amd64; sum="$SHA256_amd64" ;; \
      arm64|aarch64) arch=arm64; sum="$SHA256_arm64" ;; \
      *) echo "no pinned mc binary for $arch" >&2; exit 1 ;; \
    esac; \
    wget -q -O /usr/bin/mc "https://github.com/minio/mc/releases/download/$RELEASE/mc.linux-$arch.$RELEASE"; \
    echo "$sum  /usr/bin/mc" | sha256sum -c -; \
    chmod 0755 /usr/bin/mc
LABEL org.opencontainers.image.source=https://github.com/minio/mc \
      org.opencontainers.image.version=$RELEASE
ENTRYPOINT ["/usr/bin/mc"]
