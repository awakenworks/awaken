#!/usr/bin/env bash
# Image staging reuses the same Cargo artifact and private-output snapshot owner
# as E2E executables. No shared-target path inference or second JSON parser.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../.." && pwd)
staged=${1:?usage: stage-binary.sh OUTPUT [FEATURES]}
features=${2:-}
# Preserve caller-relative output paths before entering the repository.
[[ "$staged" = /* ]] || staged="$PWD/$staged"
args=(awaken-sandbox awaken-sandbox --release --snapshot-destination "$staged")
[[ -z "$features" ]] || args+=(--features "$features")
cd "$repo"
node e2e/cargo_binary.mjs "${args[@]}" >/dev/null
