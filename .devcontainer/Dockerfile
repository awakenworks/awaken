FROM node:22-bookworm-slim@sha256:c3de60bf2f9dd0ac6370e6117950ff62d6e339527e7472301c9c78a017978392 AS node
FROM rust:1.96.0-bookworm@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc
COPY --from=node /usr/local/ /usr/local/
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-venv clang cmake pkg-config libssl-dev postgresql-client \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy llvm-tools-preview \
    && corepack enable pnpm
WORKDIR /workspaces/awaken
CMD ["sleep", "infinity"]
