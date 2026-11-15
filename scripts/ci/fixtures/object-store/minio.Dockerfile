# The MinIO server the PostgreSQL gate's object store runs.
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
ARG RELEASE=RELEASE.2024-12-18T13-15-44Z
ARG SHA256_amd64=88182336ab5793488f2caad54846056b330f0dcb5c488b60d52434beeebae3ca
ARG SHA256_arm64=455c9b3093b7c1e6153c85cfd745c7c401a92497346ba24dbe45775c7b3ab1cd
RUN set -eu; \
    arch="${TARGETARCH:-$(uname -m)}"; \
    case "$arch" in \
      amd64|x86_64) arch=amd64; sum="$SHA256_amd64" ;; \
      arm64|aarch64) arch=arm64; sum="$SHA256_arm64" ;; \
      *) echo "no pinned minio binary for $arch" >&2; exit 1 ;; \
    esac; \
    wget -q -O /usr/bin/minio "https://github.com/minio/minio/releases/download/$RELEASE/minio.linux-$arch.$RELEASE"; \
    echo "$sum  /usr/bin/minio" | sha256sum -c -; \
    chmod 0755 /usr/bin/minio
LABEL org.opencontainers.image.source=https://github.com/minio/minio \
      org.opencontainers.image.version=$RELEASE
ENTRYPOINT ["/usr/bin/minio"]
